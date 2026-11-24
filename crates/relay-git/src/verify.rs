//! 受控验证（08 T10、06 §4）：任务只传已批准的规则 ID，可执行文件与
//! argv 一律从可信配置解析。未知 ID、参数覆盖与模型给出的命令没有入口；
//! 经 shell 与环境继承被显式排除；executable 强制绝对路径。
//!
//! 工作目录为受管 worktree。运行期间由读取线程持续消费 stdout/stderr
//! （缓冲有界：每流只保留前 2 KiB），管道满不会把正常退出误判为超时；
//! 进程结束后等待管道 EOF 有上限（孙进程可能继承输出管道），不无限
//! 等待。超时到期终止直接子进程；进程组回收属于 WP3。输出摘要去除
//! ANSI、转义不可打印字符、过滤明显凭据；按 UTF-8 字节计，含截断标记
//! 不超过 2 KiB。原始输出不持久化。

use std::fmt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// 每条规则输出摘要上限（06：每条输出摘要最多 2 KiB，按 UTF-8 字节计，
/// 含截断标记）。
pub const MAX_OUTPUT_EXCERPT_BYTES: usize = 2048;

/// 单流管道读取缓冲上限：摘要只需头部，内存有界。
const STREAM_BUFFER_BYTES: usize = 2048;

/// 可信验证规则：固定的 executable、argv 与超时。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRule {
    /// 已批准的规则 ID（任务只能引用，不能改写内容）。
    pub id: String,
    /// 固定可执行文件，强制绝对路径（子进程不继承 PATH）。
    pub executable: String,
    /// 固定 argv（不接受参数覆盖；允许为空）。
    pub argv: Vec<String>,
    /// 超时毫秒数；到期终止直接子进程。
    pub timeout_ms: u64,
}

/// 可信验证配置：规则集合，ID 唯一。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyConfig {
    rules: Vec<VerifyRule>,
}

impl VerifyConfig {
    /// 从可信配置构建；executable 必须是绝对路径，ID 重复、ID 为空或
    /// 超时为零即拒绝。argv 允许为空（如 /usr/bin/true），内容不可被
    /// 任务覆盖。
    pub fn new(rules: Vec<VerifyRule>) -> Result<VerifyConfig, VerifyError> {
        let mut seen: Vec<&str> = Vec::new();
        for rule in &rules {
            if rule.id.is_empty() {
                return Err(VerifyError::InvalidRule {
                    rule_id: rule.id.clone(),
                    reason: "规则 ID 为空".into(),
                });
            }
            if !Path::new(&rule.executable).is_absolute() {
                return Err(VerifyError::InvalidRule {
                    rule_id: rule.id.clone(),
                    reason: "executable 必须是绝对路径".into(),
                });
            }
            if rule.timeout_ms == 0 {
                return Err(VerifyError::InvalidRule {
                    rule_id: rule.id.clone(),
                    reason: "超时必须为正数".into(),
                });
            }
            if seen.contains(&rule.id.as_str()) {
                return Err(VerifyError::DuplicateRuleId {
                    rule_id: rule.id.clone(),
                });
            }
            seen.push(&rule.id);
        }
        Ok(VerifyConfig { rules })
    }

    /// 按已批准的规则 ID 解析；未知或未授权的 ID 一律拒绝。
    pub fn resolve(&self, rule_id: &str) -> Result<&VerifyRule, VerifyError> {
        self.rules
            .iter()
            .find(|r| r.id == rule_id)
            .ok_or_else(|| VerifyError::UnknownRule {
                rule_id: rule_id.to_owned(),
            })
    }
}

/// 单条验证结果状态。`NotRun` 表示规则未能启动（启动失败等非缺失工具
/// 的情形）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    /// 退出码 0。
    Passed,
    /// 退出码非零。
    Failed,
    /// 超时（直接子进程已终止）。
    Timeout,
    /// 工具未找到（规则未启动）。
    ToolNotFound,
    /// 启动失败的其他情形（规则未启动）。
    NotRun,
}

/// 单条验证结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutcome {
    pub rule_id: String,
    pub executable: String,
    /// 实际执行的固定 argv（显示用，安全）。
    pub argv: Vec<String>,
    pub status: VerificationStatus,
    /// 退出码；超时或未启动时为 None。
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    /// 清洗后的受限输出摘要（stdout 后接 stderr，含截断标记 ≤ 2 KiB）。
    pub output_excerpt: String,
    /// 未启动或失败原因（稳定文本）。
    pub not_run_reason: Option<String>,
}

/// 受控验证错误。
#[derive(Debug)]
pub enum VerifyError {
    /// 未知或未授权的规则 ID。
    UnknownRule {
        rule_id: String,
    },
    /// 规则 ID 重复。
    DuplicateRuleId {
        rule_id: String,
    },
    /// 规则定义不合法。
    InvalidRule {
        rule_id: String,
        reason: String,
    },
    /// 工作目录无效。
    WorkdirInvalid {
        path: PathBuf,
    },
    /// 工具未找到（可执行文件无法启动）。
    ToolNotFound {
        rule_id: String,
        executable: String,
    },
    /// 进程启动失败的其他情形（不中断收据流程的场景由调用层转换）。
    SpawnFailed {
        rule_id: String,
        reason: String,
    },
    /// The supervising attempt requested no further verification.
    Stopped,
    ProcessFailed(String),
    /// A started process could not be confirmed gone; never start the next rule.
    ExitUnconfirmed(String),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::UnknownRule { rule_id } => {
                write!(f, "规则 {rule_id} 不在可信配置中（未授权或未知）")
            }
            VerifyError::DuplicateRuleId { rule_id } => {
                write!(f, "规则 ID {rule_id} 重复")
            }
            VerifyError::InvalidRule { rule_id, reason } => {
                write!(f, "规则 {rule_id} 不合法：{reason}")
            }
            VerifyError::WorkdirInvalid { path } => {
                write!(f, "工作目录 {} 不是目录", path.display())
            }
            VerifyError::ToolNotFound {
                rule_id,
                executable,
            } => {
                write!(f, "规则 {rule_id} 的工具 {executable} 未找到")
            }
            VerifyError::Stopped => f.write_str("验证已停止"),
            VerifyError::ProcessFailed(reason) => write!(f, "验证进程异常：{reason}"),
            VerifyError::ExitUnconfirmed(reason) => write!(f, "验证进程退出未确认：{reason}"),
            VerifyError::SpawnFailed { rule_id, reason } => {
                write!(f, "规则 {rule_id} 的进程启动失败：{reason}")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// 去除 ANSI 转义序列（CSI 与 OSC 的常见形态）。
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                // CSI：参数 0x30–0x3F、中间 0x20–0x2F、终结 0x40–0x7E。
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if (0x40..=0x7E).contains(&(n as u32)) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC：到 BEL 或 ST（ESC \）。
                chars.next();
                let mut prev_bel = false;
                for n in chars.by_ref() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        prev_bel = true;
                        continue;
                    }
                    if prev_bel && n == '\\' {
                        break;
                    }
                    prev_bel = false;
                }
            }
            _ => {}
        }
    }
    out
}

/// 不可打印字符转义（保留换行与 Tab）。
fn escape_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let cp = c as u32;
        if c == '\n' || c == '\t' {
            out.push(c);
        } else if cp < 0x20 || cp == 0x7F {
            out.push_str(&format!("\\u{{{:02x}}}", cp));
        } else {
            out.push(c);
        }
    }
    out
}

/// 过滤明显凭据：行内出现 password/passwd/token/secret/api_key/
/// authorization 等键，键与 `=` / `:` 之间允许常见空格与 Tab；按实际
/// 匹配位置过滤（无关的更早分隔符不会截断匹配），值连同行尾一并替换。
fn redact_credentials(text: &str) -> String {
    text.lines().map(redact_line).collect::<Vec<_>>().join("\n")
}

fn redact_line(line: &str) -> String {
    const KEYS: [&str; 6] = [
        "password",
        "passwd",
        "token",
        "secret",
        "api_key",
        "authorization",
    ];
    // ASCII 大小写匹配在原始字节上进行：全量小写会改变 Unicode 前缀的
    // 字节长度，匹配位置将无法用于切片原文。匹配到的键均为 ASCII，
    // eq_ignore_ascii_case 折叠不影响非 ASCII 前缀的字节位置。
    let bytes = line.as_bytes();
    // 最早的（键, 值起点）匹配：键后允许空格/Tab 再接 `=` 或 `:`。
    let mut best: Option<(usize, usize)> = None;
    for key in KEYS {
        let key_bytes = key.as_bytes();
        let mut from = 0usize;
        while from + key_bytes.len() <= bytes.len() {
            if bytes[from..from + key_bytes.len()].eq_ignore_ascii_case(key_bytes) {
                let key_end = from + key_bytes.len();
                let mut sep = key_end;
                while sep < bytes.len() && (bytes[sep] == b' ' || bytes[sep] == b'\t') {
                    sep += 1;
                }
                if sep < bytes.len() && (bytes[sep] == b'=' || bytes[sep] == b':') {
                    // 值起点越过分隔符后的空格，前缀保留 "键 = " 的空格。
                    let mut value_start = sep + 1;
                    while value_start < bytes.len()
                        && (bytes[value_start] == b' ' || bytes[value_start] == b'\t')
                    {
                        value_start += 1;
                    }
                    if best.map(|(s, _)| from < s).unwrap_or(true) {
                        best = Some((from, value_start));
                    }
                    break;
                }
                from = key_end;
            } else {
                from += 1;
            }
        }
    }
    let Some((_key_start, value_start)) = best else {
        return line.to_owned();
    };
    // 值延伸到行尾：凭据行整段替换，无关的更早分隔符不会留下凭据值。
    format!("{}[已过滤]", &line[..value_start])
}

/// 摘要清洗：去 ANSI → 转义不可打印 → 过滤明显凭据 → 截断（含标记
/// 不超过 2 KiB）。
fn clean_excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    let mut raw = String::from_utf8_lossy(stdout).into_owned();
    let err_text = String::from_utf8_lossy(stderr);
    if !err_text.is_empty() {
        if !raw.is_empty() {
            raw.push('\n');
        }
        raw.push_str(&err_text);
    }
    let cleaned = raw
        .lines()
        .map(|line| {
            let stripped = strip_ansi(line);
            let escaped = escape_control(&stripped);
            redact_credentials(&escaped)
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate_with_marker(&cleaned)
}

/// 截断：按 UTF-8 字节计，含截断标记不超过上限。
fn truncate_with_marker(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_EXCERPT_BYTES {
        return text.to_owned();
    }
    const MARKER: &str = "…[截断]";
    let budget = MAX_OUTPUT_EXCERPT_BYTES - MARKER.len();
    let mut end = budget;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], MARKER)
}

/// 管道读取线程：持续消费直到 EOF，缓冲只保留前 2 KiB（内存有界），
/// 结束时发出信号。孙进程继承管道不会造成无期限等待——主流程只等待
/// 有上限的信号窗口，超窗即用已缓冲内容。
fn spawn_pipe_reader(
    pipe: impl std::io::Read + Send + 'static,
) -> (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>) {
    let buffer = Arc::new(Mutex::new(Vec::with_capacity(STREAM_BUFFER_BYTES)));
    let (tx, rx) = mpsc::channel();
    let buffer_for_thread = Arc::clone(&buffer);
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut chunk = [0u8; 512];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    // 阻塞 read 期间不持锁：逐块短锁写入有界缓冲，
                    // 主流程的 EOF 等待窗口过后也能立刻取到快照。
                    let mut buffer = buffer_for_thread
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if buffer.len() < STREAM_BUFFER_BYTES {
                        let remaining = STREAM_BUFFER_BYTES - buffer.len();
                        buffer.extend_from_slice(&chunk[..n.min(remaining)]);
                    }
                    // 超出部分丢弃：摘要只需头部，内存有界。
                }
                Err(_) => break,
            }
        }
        drop(tx);
    });
    (buffer, rx)
}

/// 在受管 worktree 中以固定 executable/argv 运行一条已批准的规则。
///
/// 进程直接启动（不经 shell）、环境完全清空（不继承敏感环境，也不继承
/// PATH——可执行文件须为绝对路径）、工作目录为受管 worktree。运行期间
/// 读取线程持续消费管道（内存有界），正常退出不会被管道满误判为超时；
/// 超时到期终止直接子进程（进程组回收属 WP3），随后等待管道 EOF 的
/// 窗口有上限——孙进程继承输出管道不会造成无期限等待。
pub fn run_rule(
    config: &VerifyConfig,
    rule_id: &str,
    worktree: &Path,
) -> Result<VerificationOutcome, VerifyError> {
    run_rule_inner(config, rule_id, worktree, None)
}

/// The daemon persists ownership before waiting and keeps the dispatch gate
/// until the process group and direct child are both confirmed gone.
pub trait RuleSupervisor {
    fn check_before_spawn(&self) -> Result<(), VerifyError>;
    /// Return None only for a timeout whose processes have all been reaped.
    fn wait_for_exit(
        &self,
        child: &mut Child,
        deadline: Instant,
    ) -> Result<Option<ExitStatus>, VerifyError>;
}

pub fn run_rule_supervised(
    config: &VerifyConfig,
    rule_id: &str,
    worktree: &Path,
    supervisor: &impl RuleSupervisor,
) -> Result<VerificationOutcome, VerifyError> {
    run_rule_inner(config, rule_id, worktree, Some(supervisor))
}

fn run_rule_inner(
    config: &VerifyConfig,
    rule_id: &str,
    worktree: &Path,
    supervisor: Option<&dyn RuleSupervisor>,
) -> Result<VerificationOutcome, VerifyError> {
    let rule = config.resolve(rule_id)?;
    if !worktree.is_dir() {
        return Err(VerifyError::WorkdirInvalid {
            path: worktree.to_owned(),
        });
    }
    let started = Instant::now();
    let mut command = Command::new(&rule.executable);
    if let Some(supervisor) = supervisor {
        supervisor.check_before_spawn()?;
        command.process_group(0);
    }
    let mut child = command
        .args(&rule.argv)
        .current_dir(worktree)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VerifyError::ToolNotFound {
                    rule_id: rule.id.clone(),
                    executable: rule.executable.clone(),
                }
            } else {
                VerifyError::SpawnFailed {
                    rule_id: rule.id.clone(),
                    reason: e.to_string(),
                }
            }
        })?;
    // 读取线程立即接管两条管道：持续消费，缓冲有界。
    let (stdout_buffer, stdout_done) =
        spawn_pipe_reader(child.stdout.take().expect("stdout 已设为 piped"));
    let (stderr_buffer, stderr_done) =
        spawn_pipe_reader(child.stderr.take().expect("stderr 已设为 piped"));

    let deadline = started + Duration::from_millis(rule.timeout_ms);
    let status = match supervisor {
        Some(supervisor) => supervisor.wait_for_exit(&mut child, deadline)?,
        None => wait_direct_child(&mut child, rule_id, deadline)?,
    };
    let timed_out = status.is_none();
    // 管道 EOF 等待窗口两路共用一个 200 ms 上限：孙进程继承输出管道
    // 时不无限等待；窗口过后经有界快照取已收集输出（读取线程不持锁，
    // 此处不会阻塞在 read 上）。
    let eof_deadline = Instant::now() + Duration::from_millis(200);
    let _ = stdout_done.recv_timeout(eof_deadline.saturating_duration_since(Instant::now()));
    let _ = stderr_done.recv_timeout(eof_deadline.saturating_duration_since(Instant::now()));
    let stdout_bytes: Vec<u8> = stdout_buffer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let stderr_bytes: Vec<u8> = stderr_buffer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let exit_code = status.and_then(|status| status.code());
    let outcome_status = if timed_out {
        VerificationStatus::Timeout
    } else if exit_code == Some(0) {
        VerificationStatus::Passed
    } else {
        VerificationStatus::Failed
    };
    Ok(VerificationOutcome {
        rule_id: rule.id.clone(),
        executable: rule.executable.clone(),
        argv: rule.argv.clone(),
        status: outcome_status,
        exit_code,
        timed_out,
        elapsed_ms,
        output_excerpt: clean_excerpt(&stdout_bytes, &stderr_bytes),
        not_run_reason: None,
    })
}

fn wait_direct_child(
    child: &mut Child,
    rule_id: &str,
    deadline: Instant,
) -> Result<Option<ExitStatus>, VerifyError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() >= deadline => {
                child
                    .kill()
                    .or_else(|e| {
                        if e.kind() == std::io::ErrorKind::InvalidInput {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })
                    .map_err(|e| VerifyError::ExitUnconfirmed(e.to_string()))?;
                let reap_deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return Ok(None),
                        Ok(None) if Instant::now() < reap_deadline => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        _ => return Err(VerifyError::ExitUnconfirmed("直接子进程未能回收".into())),
                    }
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(VerifyError::ExitUnconfirmed(format!("规则 {rule_id}：{e}"))),
        }
    }
}
