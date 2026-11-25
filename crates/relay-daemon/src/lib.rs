//! Agent Relay WP3-A2a/A2b：daemon 启动骨架与 IPC 接线（02/03/17）。
//!
//! 启动顺序：核对实例目录与所有权 → 取得进程生命周期内持有的独占
//! 锁 → 打开 relay-store → 显式恢复 → 绑定实例目录内的 Unix socket
//! → ready。锁争用、目录不合法、schema、恢复或绑定失败时不宣告
//! ready（返回错误）。恢复的活动任务保持 interrupted，不重新派发。
//! IPC 会话（限长帧、握手、只读 status 替身）见 [`ipc`]；worker 与
//! 本机配置不在本包。

#![forbid(unsafe_code)]

pub mod adapter_sample;
mod claude_review;
pub mod claude_wire;
pub mod ipc;
pub mod live_routes;
mod managed_edits;
pub mod model_catalog;
pub mod opencode_review;
mod route_payload;
pub mod route_pilot;
pub mod service;
mod verify_process;
pub mod workflow;

use std::fmt;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rustix::process::geteuid;
use std::path::{Path, PathBuf};

use relay_store::RecoveryRecord;

use crate::service::M1Service;

/// 恢复完成后 daemon 就绪状态：持有锁文件句柄、store 与 Unix socket
/// listener，生命周期内不释放。Drop 时先停 accept 循环、删除自己
/// 绑定的 socket 文件，再按声明顺序释放（store 先于锁）。
pub struct Daemon {
    store: Arc<relay_store::Store>,
    recovered: Vec<RecoveryRecord>,
    instance_dir: PathBuf,
    ipc_config: ipc::IpcConfig,
    socket_path: PathBuf,
    listener: UnixListener,
    ipc_stop: Arc<AtomicBool>,
    ipc_thread: Option<JoinHandle<()>>,
    /// M1 业务服务（None = 骨架模式，只读 status 替身）。
    m1: Option<Arc<M1Service>>,
    /// worker 线程控制（M1 模式）。
    worker_stop: Arc<AtomicBool>,
    worker_thread: Option<JoinHandle<()>>,
    /// 业务 handler 调用计数（供拒绝路径零调用断言）。
    business_calls: Arc<AtomicUsize>,
    /// 绑定时 socket 文件的设备号与 inode：退出时确认归属。
    socket_dev: u64,
    socket_ino: u64,
    /// 锁句柄最后释放：持有至 store 关闭之后（macOS 关闭 daemon.lock
    /// 的任一 fd 都会释放进程的全部 flock，句柄必须存活至 Drop）。
    _lock: fs::File,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Daemon")
            .field("instance_dir", &self.instance_dir)
            .field("recovered_count", &self.recovered.len())
            .field("socket_path", &self.socket_path)
            .finish()
    }
}

impl Daemon {
    /// 实例目录。
    pub fn instance_dir(&self) -> &Path {
        &self.instance_dir
    }

    /// 启动时显式恢复的任务记录（活动状态 → interrupted）。
    pub fn recovered(&self) -> &[RecoveryRecord] {
        &self.recovered
    }

    /// 就绪后的 store 访问（供后续任务包集成）。
    pub fn store(&self) -> &relay_store::Store {
        &self.store
    }

    /// store 的共享句柄（M1 service 与 daemon 同一连接：主库单 writer）。
    pub fn store_arc(&self) -> Arc<relay_store::Store> {
        Arc::clone(&self.store)
    }

    /// 实例目录内的 IPC socket 路径。
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// 启动后台 accept 循环（每连接一线程、只读 status 替身）。真实
    /// 部署改用 [`Daemon::serve_forever`]；Drop 会停止循环并按归属
    /// 确认清理 socket 文件。
    pub fn serve_background(&mut self) {
        let listener = match self.listener.try_clone() {
            Ok(l) => l,
            Err(_) => return,
        };
        let stop = Arc::clone(&self.ipc_stop);
        let config = self.ipc_config.clone();
        let calls = Arc::clone(&self.business_calls);
        // 骨架模式：只读 status 替身；M1 模式：业务分派器。
        let handler: Arc<Mutex<dyn relay_protocol::RequestHandler + Send>> = match &self.m1 {
            Some(service) => {
                let svc = Arc::clone(service);
                Arc::new(Mutex::new(ipc::FnHandler(
                    move |method: &str,
                          params: Option<&serde_json::Value>|
                          -> Option<serde_json::Value> {
                        svc.dispatch(method, params)
                    },
                )))
            }
            None => Arc::new(Mutex::new(ipc::ReadOnlyStatusHandler)),
        };
        self.ipc_thread = Some(std::thread::spawn(move || {
            ipc::accept_loop(
                listener,
                || stop.load(Ordering::SeqCst),
                config,
                handler,
                calls,
            );
        }));
    }

    /// 前台 accept 循环（真实部署）：阻塞至 Drop/停止标志。
    pub fn serve_forever(&mut self) {
        self.serve_until(|| false);
    }

    /// Foreground shutdown checks run outside the signal handler and allow Drop
    /// to finish supervised work and clear the socket while holding the lock.
    pub fn serve_until(&mut self, stopping: impl Fn() -> bool) {
        let listener = match self.listener.try_clone() {
            Ok(l) => l,
            Err(_) => return,
        };
        let calls = Arc::clone(&self.business_calls);
        let handler: Arc<Mutex<dyn relay_protocol::RequestHandler + Send>> = match &self.m1 {
            Some(service) => {
                let svc = Arc::clone(service);
                Arc::new(Mutex::new(ipc::FnHandler(
                    move |method: &str,
                          params: Option<&serde_json::Value>|
                          -> Option<serde_json::Value> {
                        svc.dispatch(method, params)
                    },
                )))
            }
            None => Arc::new(Mutex::new(ipc::ReadOnlyStatusHandler)),
        };
        ipc::accept_loop(
            listener,
            || stopping() || self.ipc_stop.load(Ordering::SeqCst),
            self.ipc_config.clone(),
            handler,
            calls,
        );
    }

    /// 业务 handler 已被调用的次数（拒绝路径应为 0）。
    pub fn business_calls(&self) -> usize {
        self.business_calls.load(Ordering::SeqCst)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // 先停 accept 循环与 worker 线程，再按归属确认清理 socket；
        // 锁与 store 按字段声明顺序释放。
        self.ipc_stop.store(true, Ordering::SeqCst);
        self.worker_stop.store(true, Ordering::SeqCst);
        if let Some(service) = &self.m1 {
            service.stop_admissions();
        }
        if let Some(handle) = self.ipc_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
        // 只删除能确认归属的端点：路径仍指向本 daemon 绑定时创建的
        // 同一个文件（dev/ino 相同）才删除。已被其他监听器重新绑定
        // （或路径消失）时不删，报出原因；拿不准一律保留。
        match fs::symlink_metadata(&self.socket_path) {
            Ok(meta) if meta.dev() == self.socket_dev && meta.ino() == self.socket_ino => {
                let _ = fs::remove_file(&self.socket_path);
                let _ = fs::remove_file(socket_own_record_path(&self.instance_dir));
            }
            Ok(_) => {
                eprintln!(
                    "relay daemon 退出：socket {} 已被重新绑定（非本 daemon 创建），保留不删",
                    self.socket_path.display()
                );
            }
            Err(_) => {}
        }
        // Explicitly release the instance flock before the File field is
        // dropped. This makes a same-process stop/restart deterministic even
        // if a short-lived child briefly inherited the descriptor.
        let _ = fs2::FileExt::unlock(&self._lock);
    }
}

/// 启动失败。任何变体都意味着 daemon 未就绪。
#[derive(Debug)]
pub enum DaemonError {
    /// 实例目录不存在、不是目录、是符号链接或权限不符合 0700 约束。
    InstanceDirInvalid { path: PathBuf, reason: String },
    /// 实例目录所有者不是当前用户（02：双端校验同 UID）。
    NotOwnedByCurrentUser { path: PathBuf, owner_uid: u32 },
    /// 实例锁被其他进程持有（02：单实例）。
    LockContended { path: PathBuf },
    /// SQLite/锁路径是符号链接（02：拒绝符号链接）。
    SymlinkRejected { path: PathBuf },
    /// store 打开失败（schema、损坏、WAL 等）。
    Store(relay_store::StoreError),
    /// 显式恢复失败。
    Recovery(relay_store::StoreError),
    /// 路径元数据读取失败。路径检查只把 NotFound 当作尚未创建；其余
    /// 读取错误一律中止启动。
    PathRead {
        path: PathBuf,
        source: std::io::Error,
    },
    /// IPC socket 路径已存在且不能确认归属：不抢占、不删除。reason 给
    /// 出可操作的处置信息（活跃端点/身份不符/记录缺失/无法确认）。
    SocketPathOccupied { path: PathBuf, reason: String },
    /// Unix socket 绑定失败：不宣告 ready。
    IpcBind {
        path: PathBuf,
        source: std::io::Error,
    },
    /// IPC 会话内部错误（接线层不应到达）。
    #[allow(dead_code)]
    IpcInternal,
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonError::InstanceDirInvalid { path, reason } => {
                write!(f, "实例目录 {} 不合法：{}", path.display(), reason)
            }
            DaemonError::NotOwnedByCurrentUser { path, owner_uid } => {
                write!(
                    f,
                    "实例目录 {} 属主 uid {owner_uid} 与当前用户不符",
                    path.display()
                )
            }
            DaemonError::LockContended { path } => {
                write!(f, "实例锁 {} 被其他进程持有", path.display())
            }
            DaemonError::SymlinkRejected { path } => {
                write!(f, "路径 {} 是符号链接，拒绝", path.display())
            }
            DaemonError::Store(e) => write!(f, "store 打开失败，未就绪：{e}"),
            DaemonError::Recovery(e) => write!(f, "显式恢复失败，未就绪：{e}"),
            DaemonError::PathRead { path, source } => {
                write!(
                    f,
                    "路径 {} 元数据读取失败，中止启动：{source}",
                    path.display()
                )
            }
            DaemonError::SocketPathOccupied { path, reason } => {
                write!(
                    f,
                    "IPC socket 路径 {} 已存在，不抢占、不删除：{}",
                    path.display(),
                    reason
                )
            }
            DaemonError::IpcBind { path, source } => {
                write!(
                    f,
                    "IPC socket {} 绑定失败，未就绪：{source}",
                    path.display()
                )
            }
            DaemonError::IpcInternal => write!(f, "IPC 内部错误"),
        }
    }
}

impl std::error::Error for DaemonError {}

/// socket 归属记录路径：绑定成功后写入 socket 文件的 dev/ino，作为
/// "本实例先前创建" 的持久凭据。
fn socket_own_record_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("relay.sock.own")
}

/// 读取归属记录；返回 (dev, ino)。
fn read_own_record(path: &Path) -> Option<(u64, u64)> {
    let text = fs::read_to_string(path).ok()?;
    let (dev, ino) = text.trim().split_once(':')?;
    Some((dev.parse().ok()?, ino.parse().ok()?))
}

/// 判定路径上的 socket 是否为"本实例先前创建、现已无监听者"的端点：
/// 常规 socket 文件、属主为当前 UID、归属记录存在且与当前文件的
/// dev/ino 一致、连接被拒（无存活监听者）。四者缺一即不能确认——
/// 活跃端点、身份不符、记录缺失或无法确认一律保留。
fn is_recyclable_own_socket(path: &Path, meta: &fs::Metadata, record: &Path) -> bool {
    // socket 文件类型：st_mode 的 S_IFSOCK（0o140000）。
    if (meta.mode() & 0o170000) != 0o140000 {
        return false;
    }
    if meta.uid() != geteuid().as_raw() {
        return false;
    }
    let (dev, ino) = match read_own_record(record) {
        Some(pair) => pair,
        None => return false,
    };
    if meta.dev() != dev || meta.ino() != ino {
        return false;
    }
    match std::os::unix::net::UnixStream::connect(path) {
        // 连接被拒：无监听者（本实例崩溃残留）。ECONNREFUSED。
        Err(e) if e.raw_os_error() == Some(libc::ECONNREFUSED) => true,
        _ => false,
    }
}

/// 启动 daemon（默认 IPC 配置）。见 [`start_with_config`]。
pub fn start(instance_dir: &Path, now: &str) -> Result<Daemon, DaemonError> {
    start_with_config(instance_dir, now, ipc::IpcConfig::default())
}

/// 启动 M1 daemon：骨架顺序 + 业务服务 + 串行 worker 线程。锁与恢复
/// 顺序沿用 A2a；worker 在锁与恢复完成后才领取（03：单机串行）。
pub fn start_m1(
    instance_dir: &Path,
    now: &str,
    ipc_config: ipc::IpcConfig,
    service_config: service::ServiceConfig,
) -> Result<Daemon, DaemonError> {
    let mut daemon = start_with_config(instance_dir, now, ipc_config)?;
    // 与 daemon 同一 store 连接（主库单 writer，03/T12）。
    let service = M1Service::for_instance(daemon.store_arc(), service_config, instance_dir);
    daemon.m1 = Some(Arc::clone(&service));
    if service.is_archive() {
        return Ok(daemon);
    }
    let stop = Arc::clone(&daemon.worker_stop);
    let worker = Arc::clone(&service);
    daemon.worker_thread = Some(std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let worked = worker.run_one_claim(&stop);
            if !worked {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }));
    Ok(daemon)
}

/// Inspect a registered project over the local socket without starting a worker.
pub fn start_project_inspection(
    instance_dir: &Path,
    now: &str,
    config: service::ServiceConfig,
) -> Result<Daemon, DaemonError> {
    let mut daemon = start(instance_dir, now)?;
    daemon.m1 = Some(M1Service::for_project_inspection(
        daemon.store_arc(),
        config,
        instance_dir,
    ));
    Ok(daemon)
}

/// 实例独占锁（daemon 与离线维护入口共用同一套规则，顺序不可调换）：
/// 1. 实例目录核对：存在、真实目录（非符号链接）、属主为当前有效 UID、
///    权限严格 0700（含组/其他位或特殊权限位即拒绝）；
/// 2. 独占锁：以不跟随符号链接的方式打开 `daemon.lock`（O_NOFOLLOW、
///    0600、不截断），打开后核对常规文件、当前有效 UID 与 0600 权限；
///    非阻塞争用即拒绝（活跃 daemon 持锁时不等待）。
///
/// 返回锁句柄：调用方持有至操作结束，Drop 时释放。macOS 上关闭
/// daemon.lock 的任一 fd 都会释放进程的全部 flock，句柄必须存活。
pub fn acquire_instance_lock(instance_dir: &Path) -> Result<(fs::File, PathBuf), DaemonError> {
    let meta = fs::symlink_metadata(instance_dir).map_err(|e| DaemonError::InstanceDirInvalid {
        path: instance_dir.to_owned(),
        reason: format!("无法访问：{e}"),
    })?;
    if meta.is_symlink() {
        return Err(DaemonError::InstanceDirInvalid {
            path: instance_dir.to_owned(),
            reason: "实例目录是符号链接".into(),
        });
    }
    if !meta.is_dir() {
        return Err(DaemonError::InstanceDirInvalid {
            path: instance_dir.to_owned(),
            reason: "实例路径不是目录".into(),
        });
    }
    if meta.uid() != geteuid().as_raw() {
        return Err(DaemonError::NotOwnedByCurrentUser {
            path: instance_dir.to_owned(),
            owner_uid: meta.uid(),
        });
    }
    let mode = meta.permissions().mode();
    if mode & 0o7777 != 0o700 {
        return Err(DaemonError::InstanceDirInvalid {
            path: instance_dir.to_owned(),
            reason: format!("目录权限 {mode:o} 须严格为 0700（拒绝组/其他与特殊权限位）"),
        });
    }
    let lock_path = instance_dir.join("daemon.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DaemonError::InstanceDirInvalid {
                    path: lock_path.clone(),
                    reason: format!("无法创建锁文件：{e}"),
                }
            } else if e.raw_os_error() == Some(libc::ELOOP) {
                DaemonError::SymlinkRejected {
                    path: lock_path.clone(),
                }
            } else {
                DaemonError::PathRead {
                    path: lock_path.clone(),
                    source: e,
                }
            }
        })?;
    let lock_meta = lock.metadata().map_err(|e| DaemonError::PathRead {
        path: lock_path.clone(),
        source: e,
    })?;
    if !lock_meta.is_file() {
        return Err(DaemonError::InstanceDirInvalid {
            path: lock_path.clone(),
            reason: "锁文件不是常规文件".into(),
        });
    }
    if lock_meta.uid() != geteuid().as_raw() {
        return Err(DaemonError::NotOwnedByCurrentUser {
            path: lock_path.clone(),
            owner_uid: lock_meta.uid(),
        });
    }
    if lock_meta.permissions().mode() & 0o777 != 0o600 {
        return Err(DaemonError::InstanceDirInvalid {
            path: lock_path.clone(),
            reason: "锁文件权限须为 0600".into(),
        });
    }
    fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| DaemonError::LockContended {
        path: lock_path.clone(),
    })?;
    Ok((lock, lock_path))
}

fn validate_database_paths(instance_dir: &Path) -> Result<(), DaemonError> {
    // 3. SQLite 与侧车路径：符号链接或非常规文件拒绝；NotFound 视为
    //    尚未创建（首启无库、无 WAL/SHM），其余读取错误中止启动。
    for name in [
        "relay.sqlite3",
        "relay.sqlite3-wal",
        "relay.sqlite3-shm",
        "relay.sqlite3-journal",
    ] {
        let path = instance_dir.join(name);
        let side_meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(DaemonError::PathRead {
                    path: path.clone(),
                    source: e,
                })
            }
        };
        if side_meta.is_symlink() {
            return Err(DaemonError::SymlinkRejected { path: path.clone() });
        }
        if !side_meta.is_file() {
            return Err(DaemonError::InstanceDirInvalid {
                path: path.clone(),
                reason: "SQLite 路径不是常规文件".into(),
            });
        }
    }
    Ok(())
}

/// Stop the daemon before running the v8 to v9 upgrade. Keep the backup
/// inside the same private instance directory and refuse to replace it.
pub fn migrate_instance_v8_to_v9(instance_dir: &Path) -> Result<PathBuf, DaemonError> {
    let (_lock, _) = acquire_instance_lock(instance_dir)?;
    validate_database_paths(instance_dir)?;
    let backup = instance_dir.join("relay.sqlite3.pre-v9");
    relay_store::migrate_v8_to_v9(&instance_dir.join("relay.sqlite3"), &backup)
        .map_err(DaemonError::Store)?;
    Ok(backup)
}

/// 启动 daemon（顺序不可调换）：
/// 1. 实例目录核对：存在、真实目录（非符号链接）、属主为当前用户、
///    权限严格为 0700（含组/其他或特殊权限位即拒绝）；
/// 2. 独占锁：打开并 `flock` 实例目录内 `daemon.lock`，非阻塞争用即
///    报告 LockContended，锁句柄在 [`Daemon`] 生命周期内持有；
/// 3. SQLite 与侧车路径核对：符号链接或非常规文件拒绝；NotFound 视为
///    尚未创建，其余读取错误中止启动；
/// 4. 打开 relay-store：schema 不符、损坏等错误在此暴露；
/// 5. 显式恢复：活动状态归入 interrupted，失败即整体未就绪；
/// 6. 绑定实例目录内 `relay.sock`（已存在路径不抢占、不删除；绑定
///    后核对 socket 文件属主 UID；绑定失败不宣告 ready）。
///
/// 恢复不重新派发；业务 worker 属后续任务包。
pub fn start_with_config(
    instance_dir: &Path,
    now: &str,
    ipc_config: ipc::IpcConfig,
) -> Result<Daemon, DaemonError> {
    // 1–2. 实例目录核对 + 独占锁（与离线维护入口共用同一规则）。
    let (lock, _lock_path) = acquire_instance_lock(instance_dir)?;
    validate_database_paths(instance_dir)?;
    // 4. 打开 store（schema 校验在 open 内完成）。
    let store = Arc::new(
        relay_store::Store::open(&instance_dir.join("relay.sqlite3"))
            .map_err(DaemonError::Store)?,
    );
    // 5. 显式恢复：失败即未就绪。锁句柄持有至 store 关闭之后
    //    （字段声明顺序保证 Drop 时 store 先释放、锁最后释放）。
    let recovered = store.recover(now).map_err(DaemonError::Recovery)?;
    // 6. IPC socket：绑定在实例目录内（0700 保证仅同 UID 可连）。
    //    已存在的 socket 按受管规则处置：陈旧死 socket（连接被拒、
    //    常规 socket 文件且属主为本 UID）是本实例崩溃残留，可接管；
    //    活跃连接或归属不明（非 socket / 异属主 / 其他连接错误）一律
    //    拒绝——不抢占、不删除。
    let socket_path = instance_dir.join("relay.sock");
    match fs::symlink_metadata(&socket_path) {
        Ok(meta) if meta.is_symlink() => {
            return Err(DaemonError::SymlinkRejected {
                path: socket_path.clone(),
            })
        }
        Ok(meta) => {
            // 活跃端点：连接成功 → 拒绝（保留）。
            if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                return Err(DaemonError::SocketPathOccupied {
                    path: socket_path.clone(),
                    reason: "端点活跃（有监听者）；如属本实例请先停止其进程，或改用其他实例目录"
                        .into(),
                });
            }
            if !is_recyclable_own_socket(&socket_path, &meta, &socket_own_record_path(instance_dir))
            {
                // 身份不符 / 记录缺失 / dev-ino 不匹配 / 非常规 socket：
                // 无法证明是本实例先前创建 → 保留并给出可操作原因。
                let reason = if (meta.mode() & 0o170000) != 0o140000 {
                    "路径存在但不是 socket 文件；请人工确认后移除".to_owned()
                } else if meta.uid() != geteuid().as_raw() {
                    format!(
                        "属主 uid {} 与当前用户不符；如属本实例请以原属主清理",
                        meta.uid()
                    )
                } else {
                    "无本实例的归属记录或 dev/ino 不匹配；可能是其他程序的端点或记录缺失，请人工确认后移除"
                        .to_owned()
                };
                return Err(DaemonError::SocketPathOccupied {
                    path: socket_path.clone(),
                    reason,
                });
            }
            // 已确认归属的本实例残留：清理后重绑。
            fs::remove_file(&socket_path).map_err(|e| DaemonError::PathRead {
                path: socket_path.clone(),
                source: e,
            })?;
            let _ = fs::remove_file(socket_own_record_path(instance_dir));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(DaemonError::PathRead {
                path: socket_path.clone(),
                source: e,
            })
        }
    }
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        // 绑定失败不宣告 ready；目标文件不是本 daemon 创建的，一律保留
        //（不无条件删除）。
        DaemonError::IpcBind {
            path: socket_path.clone(),
            source: e,
        }
    })?;
    // Socket 文件必须只对当前用户可读写（实例目录已是 0700）。
    let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
    // socket 文件属主核对（服务端侧两端 UID 之一）。
    let sock_meta = fs::symlink_metadata(&socket_path).map_err(|e| {
        // 读元数据失败：本 daemon 刚创建的端点连同记录一起清理。
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(socket_own_record_path(instance_dir));
        DaemonError::PathRead {
            path: socket_path.clone(),
            source: e,
        }
    })?;
    if sock_meta.uid() != geteuid().as_raw() {
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(socket_own_record_path(instance_dir));
        return Err(DaemonError::NotOwnedByCurrentUser {
            path: socket_path.clone(),
            owner_uid: sock_meta.uid(),
        });
    }
    // 持久归属记录：本实例创建的凭据（重启时据此确认可回收）。
    let own_path = socket_own_record_path(instance_dir);
    fs::write(
        &own_path,
        format!("{}:{}", sock_meta.dev(), sock_meta.ino()),
    )
    .map_err(|e| {
        let _ = fs::remove_file(&socket_path);
        DaemonError::PathRead {
            path: own_path.clone(),
            source: e,
        }
    })?;
    let _ = fs::set_permissions(&own_path, fs::Permissions::from_mode(0o600));
    Ok(Daemon {
        instance_dir: instance_dir.to_owned(),
        store,
        recovered,
        ipc_config,
        socket_path,
        listener,
        ipc_stop: Arc::new(AtomicBool::new(false)),
        ipc_thread: None,
        m1: None,
        worker_stop: Arc::new(AtomicBool::new(false)),
        worker_thread: None,
        business_calls: Arc::new(AtomicUsize::new(0)),
        socket_dev: sock_meta.dev(),
        socket_ino: sock_meta.ino(),
        _lock: lock,
    })
}
