//! 预检与 worktree 共用的 Git 调用层：集中环境加固与只读约束。
//!
//! 加固内容（对每次调用统一生效，不依赖调用方传参）：
//! - 结构化 argv（不经 shell、不做字符串拼接）；
//! - `--no-optional-locks` 关闭可选锁；
//! - 空 `GIT_ALLOW_PROTOCOL`：禁止一切传输协议，优先于任何
//!   `protocol.allow` / `protocol.<name>.allow` 配置——单协议配置不能
//!   放开传输；
//! - `GIT_NO_LAZY_FETCH=1`：partial clone 下对象读取不触发懒取；
//! - `-c protocol.allow=never`：配置层白名单同样关闭；
//! - 移除 `GIT_DIR` 等可劫持仓库定位的环境变量，关闭终端提示。
//!
//! 子命令只读：不执行 hooks、不触发 clean/smudge 过滤器、不联网补
//! 对象、不创建 worktree、不写 index。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Upper bound for a single git command's stdout. Exceeding it fails closed
/// (the child is killed, the caller receives `overflow`), never truncating a
/// patch into a partial candidate.
pub(crate) const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
/// stderr is diagnostic only; keep it small.
pub(crate) const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;

/// 构建经环境加固的 git `Command`（仅构建，不执行）。
///
/// 使用可信绝对路径 `/usr/bin/git`（找不到即启动失败 fail closed），
/// 禁用 system/global 配置与系统级 attributes，强制关闭 fsmonitor、
/// 外部 attributes 文件与 hooks，并按前缀清除所有可注入的 `GIT_CONFIG_*`
/// 与对象目录环境变量。子命令仍只读：不执行 hooks/过滤器、不联网补对象。
pub(crate) fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        // 传输协议双重关闭：配置白名单与环境变量都不能放开。
        .arg("-c")
        .arg("protocol.allow=never")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.attributesFile=/dev/null")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name == "GIT_CONFIG_COUNT"
            || name == "GIT_CONFIG_PARAMETERS"
            || name == "GIT_EXEC_PATH"
            || name == "GIT_ALTERNATE_OBJECT_DIRECTORIES"
            || name.starts_with("GIT_CONFIG_KEY_")
            || name.starts_with("GIT_CONFIG_VALUE_")
        {
            command.env_remove(name.as_ref());
        }
    }
    command
}

/// 有界 git 执行结果：输出超过上限或等待超时都不返回部分数据供使用。
pub(crate) struct GitOutput {
    pub(crate) ok: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) overflow: bool,
    pub(crate) timed_out: bool,
}

fn read_capped(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return (buffer, false),
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > limit {
                    buffer.truncate(limit);
                    return (buffer, true);
                }
            }
            Err(_) => return (buffer, true),
        }
    }
}

/// Run git with bounded stdout/stderr, an optional stdin payload, and a
/// deadline. Overflow or timeout kills the child and sets the corresponding
/// flag; callers must treat either flag as a hard failure.
pub(crate) fn git_raw_bounded(
    root: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<GitOutput, crate::GitCheckError> {
    use std::io::Write;
    let mut command = git_command(root, args);
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| crate::GitCheckError::GitInvocation(format!("无法启动 git：{e}")))?;
    if let Some(bytes) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(bytes);
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| crate::GitCheckError::GitInvocation("git stdout 不可用".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| crate::GitCheckError::GitInvocation("git stderr 不可用".into()))?;
    let out_reader = std::thread::spawn(move || read_capped(stdout, MAX_GIT_OUTPUT_BYTES));
    let err_reader = std::thread::spawn(move || read_capped(stderr, MAX_GIT_STDERR_BYTES));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
        }
    };
    let (stdout, out_overflow) = out_reader.join().unwrap_or_default();
    let (stderr, err_overflow) = err_reader.join().unwrap_or_default();
    let ok = status.is_some_and(|status| status.success());
    Ok(GitOutput {
        ok,
        stdout,
        stderr,
        overflow: out_overflow || err_overflow,
        timed_out,
    })
}

/// 运行 git 并返回原始结果：(成功与否, 退出码, stdout, stderr)。
pub(crate) fn git_raw(
    root: &Path,
    args: &[&str],
) -> Result<(bool, i32, String, String), crate::GitCheckError> {
    let output = git_command(root, args)
        .output()
        .map_err(|e| crate::GitCheckError::GitInvocation(format!("无法启动 git：{e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    Ok((output.status.success(), code, stdout, stderr))
}

/// 期望成功的只读子命令；失败即包装为 [`crate::GitCheckError::CommandFailed`]。
pub(crate) fn git_out(root: &Path, args: &[&str]) -> Result<String, crate::GitCheckError> {
    let (ok, _code, stdout, stderr) = git_raw(root, args)?;
    if !ok {
        return Err(crate::GitCheckError::CommandFailed {
            argv: format!("git {}", args.join(" ")),
            stderr: truncate(&stderr),
        });
    }
    Ok(stdout.trim_end().to_owned())
}

/// 期望“0 = 命中、1 = 无匹配”的查询（config --get-regexp 等）；
/// 其他退出码按失败处理。
pub(crate) fn git_query(
    root: &Path,
    args: &[&str],
) -> Result<Option<String>, crate::GitCheckError> {
    let (ok, code, stdout, stderr) = git_raw(root, args)?;
    if ok {
        return Ok(Some(stdout.trim_end().to_owned()));
    }
    if code == 1 {
        return Ok(None);
    }
    Err(crate::GitCheckError::CommandFailed {
        argv: format!("git {}", args.join(" ")),
        stderr: truncate(&stderr),
    })
}

/// 截断诊断文本（06：诊断不含原文全文）。
pub(crate) fn truncate(text: &str) -> String {
    if text.len() <= 512 {
        text.to_owned()
    } else {
        let mut end = 512;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…[截断]", &text[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::git_raw;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "relay-git-layer-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 测试夹具自身的 git 调用（不走被测加固层，用于搭建场景）。
    fn setup_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} 启动失败: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    }

    /// 纯本地 partial clone 场景：单协议配置（protocol.file.allow=always）
    /// 不能放开传输——缺失对象经共享层调用必须明确失败，对象不因懒取
    /// 而出现，也不连接真实远端。
    #[test]
    fn single_protocol_config_cannot_enable_transport_for_lazy_fetch() {
        let dir = temp_dir("promisor");
        let origin = dir.join("origin");
        let client = dir.join("client");
        fs::create_dir_all(&origin).unwrap();
        setup_git(&origin, &["init", "-q", "-b", "main"]);
        fs::write(origin.join("a.txt"), "baseline\n").unwrap();
        setup_git(&origin, &["add", "a.txt"]);
        setup_git(&origin, &["commit", "-q", "-m", "baseline"]);
        // partial clone：仅取基线，client 缺后续对象。
        let clone = Command::new("git")
            .args([
                "clone",
                "-q",
                "--filter=blob:none",
                "--no-checkout",
                format!("file://{}", origin.display()).as_str(),
                client.to_str().unwrap(),
            ])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(clone.status.success(), "partial clone 失败");

        // 远端追加新提交：client 本地没有该 commit 对象。
        fs::write(origin.join("b.txt"), "later\n").unwrap();
        setup_git(&origin, &["add", "b.txt"]);
        setup_git(&origin, &["commit", "-q", "-m", "later"]);
        let missing = setup_git(&origin, &["rev-parse", "HEAD"]);
        // client 确实缺对象（plain 调用 + 禁懒取核对，不触发传输）。
        let (_, _, _, _) = git_raw(&client, &["rev-parse", "HEAD"]).unwrap();
        let direct = Command::new("git")
            .arg("-C")
            .arg(&client)
            .args(["cat-file", "-e", &missing])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .unwrap();
        assert!(!direct.status.success(), "前置核对：对象应缺失");

        // 单协议配置放开尝试 + 传统传输开关，均不能让共享层完成读取。
        setup_git(&client, &["config", "protocol.file.allow", "always"]);
        setup_git(&client, &["config", "protocol.allow", "file"]);
        let (ok, _code, stdout, _stderr) = git_raw(&client, &["cat-file", "-t", &missing]).unwrap();
        assert!(
            !ok && stdout.trim_end() != "commit",
            "懒取与传输必须被禁用（不得从 file:// 远端补对象）"
        );

        // 对象仍未出现：共享层没有触发任何补取。
        let direct = Command::new("git")
            .arg("-C")
            .arg(&client)
            .args(["cat-file", "-e", &missing])
            .env("GIT_NO_LAZY_FETCH", "1")
            .output()
            .unwrap();
        assert!(!direct.status.success(), "调用后对象仍应缺失");

        fs::remove_dir_all(&dir).ok();
    }
}
