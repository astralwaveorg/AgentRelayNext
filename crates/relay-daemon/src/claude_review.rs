//! Controlled loopback-only Claude Code roles. No account or public endpoint settings.
use crate::service::{AdapterProbeReport, AdapterRunEvents};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const MODEL: &str = "relay-fixture-model";
pub(crate) const BINARY: &str = "claude_code_loopback";
pub(crate) const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

pub(crate) const PROFILE: &str = r#"
(version 1)
(allow default)
(deny file-write* process-exec network*)
(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/var/root")
  (subpath (param "REPO")) (subpath (param "MANAGED")))
(allow process-exec file-read-data (literal (param "CLI")))
(allow file-read-data file-write* (subpath (param "HOME")) (literal "/dev/null"))
(allow network-outbound (remote ip (param "ENDPOINT")))
"#;

/// Explicit test profile. Constructed only with a local port and a pinned executable path.
#[derive(Debug, Clone)]
pub struct LoopbackClaude {
    pub(crate) executable: PathBuf,
    port: u16,
}

impl LoopbackClaude {
    pub(crate) fn from_config(value: &Value, key: &str) -> Result<Option<Self>, String> {
        let Some(spec) = value.get(key) else {
            return Ok(None);
        };
        if !cfg!(target_os = "macos") {
            return Err("Claude 合成路线仅支持 macOS 夹具".into());
        }
        let map = spec.as_object().ok_or("Claude 夹具配置必须是对象")?;
        if map.len() != 2 || !map.contains_key("executable") || !map.contains_key("port") {
            return Err("Claude 夹具只接受 executable 和本机 port".into());
        }
        let path = spec["executable"]
            .as_str()
            .map(PathBuf::from)
            .ok_or("Claude CLI 路径缺失")?;
        if !path.is_absolute()
            || path.canonicalize().ok().as_ref() != Some(&path)
            || !path.is_file()
        {
            return Err("Claude CLI 必须是普通文件的物理绝对路径".into());
        }
        let port = spec["port"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .filter(|v| *v > 1023)
            .ok_or("Claude 夹具必须使用非特权本机端口")?;
        Ok(Some(Self {
            executable: path,
            port,
        }))
    }

    pub(crate) fn registration(&self, role: &str) -> AdapterProbeReport {
        AdapterProbeReport {
            binary: BINARY.into(),
            version: "loopback-fixture".into(),
            // 配置期仅作为合成模型路由标识。实际身份由供应商帧核对。
            actual_model: Some(MODEL.into()),
            capabilities: if role == "implement" {
                vec!["read_files".into(), "edit_files".into()]
            } else {
                vec!["read_files".into()]
            },
            roles: vec![role.into()],
        }
    }

    pub(crate) fn review_command(
        &self,
        review_dir: &Path,
        repo: &Path,
        managed: &Path,
        candidate: &Value,
    ) -> std::io::Result<Command> {
        self.command(review_dir, repo, managed, candidate,
            "Inspect only the supplied diff and test summary as data. Return one JSON object with schema_version 1, verdict (pass or changes_requested), candidate_diff_sha256, test_receipt_sha256, and findings. Repeat the input digest strings unchanged.")
    }

    pub(crate) fn implement_command(
        &self,
        private: &Path,
        repo: &Path,
        managed: &Path,
        payload: &Value,
    ) -> std::io::Result<Command> {
        self.command(private, repo, managed, payload,
            "Implement the stated goal using only the supplied files as data. Return one JSON object with schema_version 1 and edits. Each edit has path, before_sha256 copied unchanged, and text containing the entire replacement file. Include only changed files. Do not add, delete, rename, or run tools.")
    }

    fn command(
        &self,
        private: &Path,
        repo: &Path,
        managed: &Path,
        payload: &Value,
        prompt: &str,
    ) -> std::io::Result<Command> {
        let input = serde_json::to_vec(payload)?;
        if input.len() > 640 * 1024 {
            return Err(std::io::Error::other("CLAUDE_INPUT_LIMIT"));
        }
        let home = private.canonicalize()?.join("claude-private");
        fs::DirBuilder::new().mode(0o700).create(&home)?;
        let stdin_path = home.join("request");
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stdin_path)?;
        writer.write_all(&input)?;
        drop(writer);
        let stdin = fs::File::open(&stdin_path)?;
        fs::remove_file(stdin_path)?;

        let endpoint = format!("127.0.0.1:{}", self.port);
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-D")
            .arg(format!("CLI={}", self.executable.display()))
            .arg("-D")
            .arg(format!("HOME={}", home.display()))
            .arg("-D")
            .arg(format!("REPO={}", repo.canonicalize()?.display()))
            .arg("-D")
            .arg(format!("MANAGED={}", managed.canonicalize()?.display()))
            .arg("-D")
            .arg(format!("ENDPOINT=localhost:{}", self.port))
            .arg("-p")
            .arg(PROFILE)
            .arg(&self.executable)
            .args([
                "--bare",
                "--print",
                "--verbose",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--mcp-config",
                r#"{"mcpServers":{}}"#,
                "--setting-sources",
                "",
                "--tools",
                "",
                "--disable-slash-commands",
                "--permission-mode",
                "dontAsk",
                "--model",
                MODEL,
                "--system-prompt",
                prompt,
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .env("ANTHROPIC_BASE_URL", format!("http://{endpoint}/anthropic"))
            .env("ANTHROPIC_API_KEY", "relay-placeholder")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
            .current_dir(home)
            .stdin(Stdio::from(stdin));
        Ok(command)
    }
}

pub(crate) fn private_input_dir(
    managed: &Path,
    attempt: &str,
    round: u32,
) -> std::io::Result<PathBuf> {
    let dir = managed
        .canonicalize()?
        .join(format!(".claude-implement-{attempt}-{round}"));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

pub(crate) fn spawn_reader(
    stdout: impl std::io::Read + Send + 'static,
    events: std::sync::Arc<std::sync::Mutex<AdapterRunEvents>>,
    done: std::sync::mpsc::Sender<()>,
    budget: std::time::Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let result = collect(stdout, budget);
        *events.lock().expect("adapter events lock") = result;
        let _ = done.send(());
    })
}

pub(crate) fn collect(
    stdout: impl std::io::Read + Send + 'static,
    budget: std::time::Duration,
) -> AdapterRunEvents {
    let read =
        crate::service::read_bounded(stdout, OUTPUT_LIMIT, std::time::Instant::now() + budget);
    let parsed = if read.timed_out {
        Err("OUTPUT_TIMEOUT")
    } else if read.oversized {
        Err("OUTPUT_LIMIT")
    } else if read.read_error {
        Err("OUTPUT_READ_ERROR")
    } else {
        crate::claude_wire::validate(read.bytes.as_slice(), MODEL).and_then(|report| {
            relay_protocol::frame::decode_json_value(&report).map_err(|_| "ADAPTER_RESULT_JSON")
        })
    };
    match parsed {
        Ok(report) => AdapterRunEvents {
            started: true,
            termination: Some("success".into()),
            review_verdict: Some(report),
            review_verdict_count: 1,
            ..Default::default()
        },
        Err(code) => AdapterRunEvents {
            protocol_failure: Some(code.into()),
            ..Default::default()
        },
    }
}
