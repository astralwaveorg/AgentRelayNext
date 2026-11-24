//! Agent Relay 平台原语的安全包装（M1 候选）。
//!
//! 集中三组内核能力，调用方（relay-git / relay-daemon）保持
//! `#![forbid(unsafe_code)]`，不直接触碰 FFI：
//!
//! - [`rename_no_replace`]：平台提供的原子"不替换"改名。macOS 用
//!   `renamex_np(RENAME_EXCL)`，目标已存在（含空目录）时原子失败
//!   且不改动目标；Linux 用 `renameat2(RENAME_NOREPLACE)`。不得以
//!   "再检查一次 + 普通 rename" 代替——那存在已复现的覆盖窗口。
//! - [`socket_peer_euid`]：内核提供的连接对端有效 UID。macOS 用
//!   `getpeereid`，Linux 用 `SO_PEERCRED`。分派业务前核对对端身份。
//! - 进程组：[`setpgid_child`] 与 [`killpg_term`]，取消只处理受管
//!   子进程自己的进程组。

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    // The handler only performs a lock-free atomic store. All cleanup happens
    // in normal Rust code, after the foreground loop observes this flag.
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Register process-wide SIGINT/SIGTERM handling before starting daemon threads.
/// Invoke once from the foreground entry point, not from library callers.
pub fn install_stop_signals() -> io::Result<()> {
    // A zeroed sigaction followed by sigemptyset and an exact C handler pointer
    // is the platform ABI for a handler without SA_SIGINFO.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_sigaction = request_stop as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        let mut old_int: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGINT, &action, &mut old_int) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            let error = io::Error::last_os_error();
            libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
            return Err(error);
        }
    }
    Ok(())
}

pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// 原子改名、目标已存在即失败（不替换、不改动目标）。
///
/// `new` 已存在时返回 [`io::ErrorKind::AlreadyExists`]；`old` 与
/// `new` 必须同文件系统。失败时 `new` 保持调用前原样——已存在的
/// 空目录不会被移走或覆盖。
pub fn rename_no_replace(old: &Path, new: &Path) -> io::Result<()> {
    let old = path_into(old)?;
    let new = path_into(new)?;
    let ret = unsafe { rename_no_replace_raw(old.as_ptr(), new.as_ptr()) };
    if ret == 0 {
        return Ok(());
    }
    Err(io::Error::last_os_error())
}

/// Atomically exchange two existing paths on the same filesystem.
pub fn exchange_paths(first: &Path, second: &Path) -> io::Result<()> {
    let first = path_into(first)?;
    let second = path_into(second)?;
    #[cfg(target_os = "macos")]
    let ret = unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(target_os = "linux")]
    let ret = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 查询已连接 Unix socket 的对端有效 UID。
///
/// `stream` 必须是已 accept/已连接的 socket。失败（如未连接）返回
/// io 错误；对端 UID 与本进程不一致时由调用方拒绝连接。
pub fn socket_peer_euid<S: AsRawFd>(stream: &S) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    peer_euid_raw(fd)
}

/// 让子进程自成进程组（pgid = 子 pid），供后续 killpg 精确回收。
///
/// 在 `spawn` 之后立即调用；父进程侧 setpgid 在子进程未 exec/未退出
/// 前有效，存在极窄竞态窗口，由调用方文档说明。
pub fn setpgid_child(child_pid: i32) -> io::Result<()> {
    let ret = unsafe { libc::setpgid(child_pid, child_pid) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 向进程组发送 SIGTERM；仅用于回收本工具确认归属的进程组。
pub fn killpg_term(pgid: i32) -> io::Result<()> {
    let ret = unsafe { libc::killpg(pgid, libc::SIGTERM) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 向进程组发送 SIGKILL（升级回收：TERM 被忽略时）。
pub fn killpg_kill(pgid: i32) -> io::Result<()> {
    let ret = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 进程组是否仍有存活成员（pgrep -g 枚举）：Ok(true)=有成员，
/// Ok(false)=组为空（全部退出），Err=无法判定（枚举失败——按不可
/// 确认处理）。macOS 上 kill(pgid,0) 对空组返回 EPERM 而非 ESRCH，
/// 不能用于可靠探测。
pub fn group_has_members(pgid: i32) -> io::Result<bool> {
    let output = Command::new("/usr/bin/pgrep")
        .arg("-g")
        .arg(pgid.to_string())
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(code) => Err(io::Error::other(format!("pgrep 退出 {code}"))),
        None => Err(io::Error::other("pgrep 被信号终止")),
    }
}

/// 路径所在文件系统的可用字节数（statvfs f_bavail × f_frsize）。
/// 供领取门（低磁盘不派发）使用。
pub fn free_space(path: &Path) -> io::Result<u64> {
    let c_path = path_into(path)?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

/// 进程组是否已无存活成员（kill 0 探测）。
pub fn killpg_probe(pgid: i32) -> io::Result<bool> {
    let ret = unsafe { libc::killpg(pgid, 0) };
    if ret == 0 {
        Ok(true)
    } else {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(err)
        }
    }
}

fn path_into(path: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL 字节"))
}

#[cfg(target_os = "macos")]
unsafe fn rename_no_replace_raw(old: *const std::ffi::c_char, new: *const std::ffi::c_char) -> i32 {
    libc::renamex_np(old, new, libc::RENAME_EXCL)
}

#[cfg(target_os = "linux")]
unsafe fn rename_no_replace_raw(old: *const std::ffi::c_char, new: *const std::ffi::c_char) -> i32 {
    libc::renameat2(
        libc::AT_FDCWD,
        old,
        libc::AT_FDCWD,
        new,
        libc::RENAME_NOREPLACE,
    )
}

#[cfg(target_os = "macos")]
fn peer_euid_raw(fd: std::ffi::c_int) -> io::Result<u32> {
    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    let ret = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
    if ret == 0 {
        Ok(euid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn peer_euid_raw(fd: std::ffi::c_int) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if ret == 0 {
        Ok(cred.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("relay-sys 需要 macOS 或 Linux 平台原语");

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "relay-sys-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 原子不替换：目标已存在（空目录）时失败且目录原样保留。
    #[test]
    fn rename_no_replace_refuses_existing_empty_dir() {
        let dir = temp_dir("rnrexcl");
        let src = dir.join("staging");
        let dst = dir.join("attempt-1");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("manifest.json"), "{}").unwrap();
        // 预先存在的空目标目录。
        fs::create_dir_all(&dst).unwrap();

        let err = rename_no_replace(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        // 目标仍是同一个空目录，未被替换或填充。
        let entries: Vec<_> = fs::read_dir(&dst).unwrap().collect();
        assert!(entries.is_empty(), "空目录不得被覆盖: {entries:?}");
        // 源目录未被动过。
        assert!(src.join("manifest.json").is_file());

        // 不存在目标时正常改名。
        rename_no_replace(&src, &dir.join("attempt-2")).unwrap();
        assert!(dir.join("attempt-2/manifest.json").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 文件目标同样拒绝替换。
    #[test]
    fn rename_no_replace_refuses_existing_file() {
        let dir = temp_dir("rnrexcl-file");
        let src = dir.join("staging");
        let dst = dir.join("attempt-1");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("x"), "a").unwrap();
        fs::write(&dst, "existing").unwrap();

        let err = rename_no_replace(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&dst).unwrap(), b"existing", "已有文件不得被替换");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 对端 UID：本机连接返回当前有效 UID（跨 UID 拒绝无环境，未测）。
    #[test]
    fn socket_peer_euid_reports_own_euid() {
        use std::os::unix::net::UnixListener;
        use std::os::unix::net::UnixStream;
        let dir = temp_dir("peeruid");
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _addr) = listener.accept().unwrap();
        let euid = socket_peer_euid(&server).unwrap();
        // rustix 的 geteuid 与 libc uid_t 同源；这里经 /proc-free 比对：
        // 直接与 libc::geteuid 等价的 rustix 包装结果比较。
        assert_eq!(euid, rustix_euid());
        drop(client);
        drop(server);
        drop(listener);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn rustix_euid() -> u32 {
        // relay-daemon 用 rustix::process::geteuid；此处经 libc 等价。
        unsafe { libc::geteuid() }
    }
}
