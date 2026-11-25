//! Explicitly approved subscription qualification samples. Never enable daemon routing.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const ENDPOINT: &str = "https://api.minimax.io/anthropic/v1/messages";
pub(crate) const CN_ENDPOINT: &str = "https://api.minimax.cn/anthropic/v1/messages";
pub(crate) const MODEL: &str = "MiniMax-M3.1-Flash";
pub(crate) const GO_ENDPOINT: &str = "https://opencode.ai/inference/go/openai/v1/chat/completions";
pub(crate) const GO_USAGE: &str = "https://opencode.ai/inference/go/v1/usage";
pub(crate) const GO_MODEL: &str = "deepseek-v4.1-flash";
const REVIEW_MODEL: &str = "MiniMax-M3";
const SAMPLE: &str = "Return precisely this ASCII text and nothing else: relay-pilot-ok";
const REVIEW_SAMPLE: &str = r#"Read-only qualification sample. Return only this JSON object without Markdown: {"verdict":"pass","sample":"relay-pilot-review-ok"}"#;
const LOCAL_PATH: &str = "/anthropic/v1/messages";
pub(crate) const LIMIT: usize = 128 * 1024;

pub(crate) type Pilot<T> = Result<T, &'static str>;

fn minimax_endpoint_value(value: Option<&str>) -> Pilot<&'static str> {
    match value {
        None | Some(ENDPOINT) => Ok(ENDPOINT),
        Some(CN_ENDPOINT) => Ok(CN_ENDPOINT),
        Some(_) => Err("MINIMAX_ENDPOINT_INVALID"),
    }
}

pub(crate) fn minimax_endpoint() -> Pilot<&'static str> {
    match std::env::var("RELAY_MINIMAX_ENDPOINT") {
        Ok(value) => minimax_endpoint_value(Some(&value)),
        Err(std::env::VarError::NotPresent) => minimax_endpoint_value(None),
        Err(std::env::VarError::NotUnicode(_)) => Err("MINIMAX_ENDPOINT_INVALID"),
    }
}

pub(crate) fn is_minimax_endpoint(endpoint: &str) -> bool {
    endpoint == ENDPOINT || endpoint == CN_ENDPOINT
}

/// Provider retry policy: 429 / transient transport only. Ten retries with a
/// 30 s interval by default; bounds keep a worker from sleeping for hours.
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 10;
pub(crate) const DEFAULT_RETRY_INTERVAL_MS: u64 = 30_000;
pub(crate) const MAX_RETRIES_LIMIT: u32 = 20;
pub(crate) const RETRY_INTERVAL_MIN_MS: u64 = 1_000;
pub(crate) const RETRY_INTERVAL_MAX_MS: u64 = 120_000;

pub(crate) fn retry_settings(
    max_retries: Option<&str>,
    interval_ms: Option<&str>,
) -> Pilot<(u32, Duration)> {
    let retries = match max_retries {
        None => DEFAULT_MAX_RETRIES,
        Some(value) => value.parse::<u32>().map_err(|_| "RETRY_SETTINGS_INVALID")?,
    };
    if retries > MAX_RETRIES_LIMIT {
        return Err("RETRY_SETTINGS_INVALID");
    }
    let interval = match interval_ms {
        None => DEFAULT_RETRY_INTERVAL_MS,
        Some(value) => value.parse::<u64>().map_err(|_| "RETRY_SETTINGS_INVALID")?,
    };
    if !(RETRY_INTERVAL_MIN_MS..=RETRY_INTERVAL_MAX_MS).contains(&interval) {
        return Err("RETRY_SETTINGS_INVALID");
    }
    Ok((retries, Duration::from_millis(interval)))
}

/// Which failures justify re-sending the same compiled sample: rate limiting
/// and transient transport errors. Protocol/model/auth failures never retry.
pub(crate) fn retryable(code: &str) -> bool {
    matches!(code, "UPSTREAM_RATE_LIMIT" | "UPSTREAM_TRANSPORT")
}

/// Fallback model for the same account/tool. Empty/unset means "no fallback".
/// It must not equal the implement model or the primary model of the stage.
pub(crate) fn fallback_model_value(value: Option<&str>, primary: &str) -> Pilot<Option<String>> {
    let Some(model) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if model.len() > 128
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        || model == MODEL
        || model == primary
    {
        return Err("FALLBACK_MODEL_INVALID");
    }
    Ok(Some(model.to_owned()))
}

pub(crate) fn go_model_value(value: Option<&str>) -> Pilot<String> {
    let model = value.unwrap_or(GO_MODEL);
    if model.is_empty()
        || model.len() > 128
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        || model == MODEL
    {
        return Err("GO_MODEL_INVALID");
    }
    Ok(model.to_owned())
}

pub(crate) fn go_model() -> Pilot<String> {
    match std::env::var("RELAY_GO_MODEL") {
        Ok(value) => go_model_value(Some(&value)),
        Err(std::env::VarError::NotPresent) => go_model_value(None),
        Err(std::env::VarError::NotUnicode(_)) => Err("GO_MODEL_INVALID"),
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for b in bytes.as_ref() {
        write!(&mut out, "{b:02x}").expect("String write");
    }
    out
}

pub(crate) fn nonce() -> Pilot<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "LOCAL_ENTROPY")?;
    Ok(hex(bytes))
}

fn request() -> Value {
    json!({
        "model":MODEL, "max_tokens":512, "stream":true, "output_config":{"effort":"low"},
        "messages":[{"role":"user","content":SAMPLE}]
    })
}

fn review_request(model: &str, go: bool) -> Value {
    if go {
        return json!({"model":model,"max_tokens":512,"stream":false,"reasoning_effort":"low",
            "messages":[{"role":"user","content":REVIEW_SAMPLE}]});
    }
    json!({
        "model":model,"max_tokens":512,"stream":true,
        "messages":[{"role":"user","content":REVIEW_SAMPLE}]
    })
}

const EDIT_FILE: &str = "status.txt";
const EDIT_BEFORE: &str = "status=waiting\n";
const EDIT_AFTER: &str = "status=ready\n";
const EDIT_GOAL: &str = "Change status from waiting to ready, preserving the final newline.";

fn edit_prompt() -> String {
    format!("Using only the supplied file, {} Return one JSON object with schema_version 1 and edits. Each edit has path, before_sha256 copied unchanged, and text containing the entire replacement file. Never run a tool. Input: {}", EDIT_GOAL,
        json!({"files":[{"path":EDIT_FILE,"before_sha256":relay_git::sha256_hex(EDIT_BEFORE.as_bytes()),"text":EDIT_BEFORE}]}))
}

fn edit_request() -> Value {
    let mut body = request();
    body["messages"][0]["content"] = edit_prompt().into();
    body
}

pub(crate) fn cli_digest(cli: &Path) -> Pilot<String> {
    let mut file = File::open(cli).map_err(|_| "CLI_NOT_READABLE")?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut bytes).map_err(|_| "CLI_NOT_READABLE")?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(hex(hash.finalize()))
}

fn plan(cli: &Path, review: bool, go: bool, edit: bool) -> Pilot<Value> {
    if !cfg!(target_os = "macos") || !cli.is_file() {
        return Err("PILOT_REQUIRES_MACOS_CLI");
    }
    let review_model = if go { Some(go_model()?) } else { None };
    let details = json!({
        "provider":if go {"OpenCode Go"} else {"MiniMax"},
        "key_kind":if go {"Console API key from the subscribed workspace"} else {"M Plan subscription key"},
        "billing":if go {"operator must confirm Extra Usage is off; check Go meters before sending and require Go billing with zero balance charge in the response"}
            else {"operator must confirm available plan and that credits are acceptable if used; actual debit unverified"},
        "endpoint":if go {GO_ENDPOINT} else {minimax_endpoint()?},
        "quota_endpoint":go.then_some(GO_USAGE),
        "request":if review { review_request(review_model.as_deref().unwrap_or(REVIEW_MODEL), go) } else if edit { edit_request() } else { request() },
        "sample":if edit { "bounded_file_edit" } else { "route_probe" },
        "role":if review {"review"} else {"implement"},
        "tool":if review {"opencode"} else {"claude-code"},
        "max_provider_requests":1,
        "fallback":false, "cli":cli, "cli_sha256":cli_digest(cli)?,
        "production_enabled":false
    });
    let canonical = serde_json::to_vec(&details).map_err(|_| "PLAN_ENCODING")?;
    Ok(json!({"approval":hex(Sha256::digest(canonical)), "details":details}))
}

pub(crate) struct Temp(pub(crate) PathBuf);
impl Temp {
    pub(crate) fn new() -> Pilot<Self> {
        let dir = Path::new("/private/tmp").join(format!("relay-pilot-{}", &nonce()?[..14]));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|_| "PILOT_TEMP")?;
        Ok(Self(dir))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ProcessScope {
    Standalone,
    Tracked,
}
impl ProcessScope {
    fn apply(self, command: &mut Command) -> &mut Command {
        if matches!(self, Self::Standalone) {
            command.process_group(0);
        }
        command
    }
}
struct ChildGroup(Child, ProcessScope);
impl ChildGroup {
    fn successful(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.0.try_wait() {
                Ok(Some(s)) => return s.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(15))
                }
                _ => return false,
            }
        }
    }
}
/// Bounded wait for a process group to disappear. Probe errors count as
/// "still alive" so they never produce an exit claim.
fn wait_group_gone(pgid: i32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        match relay_sys::group_has_members(pgid) {
            Ok(false) => return true,
            Ok(true) | Err(_) => {
                if Instant::now() >= deadline {
                    return false;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let pid = self.0.id() as i32;
        match self.1 {
            ProcessScope::Standalone => {
                // The child leads its own group; reclaim it like the daemon
                // supervisor does: TERM, then KILL, each with a bounded probe.
                let _ = relay_sys::killpg_term(pid);
                if !wait_group_gone(pid, Duration::from_millis(500)) {
                    let _ = relay_sys::killpg_kill(pid);
                    let _ = wait_group_gone(pid, Duration::from_millis(1500));
                }
            }
            ProcessScope::Tracked => {
                // Tracked children share the daemon group: only signal the
                // direct child, never the group, and reap it with a bound.
                let _ = self.0.kill();
                let deadline = Instant::now() + Duration::from_millis(2_000);
                while Instant::now() < deadline {
                    if matches!(self.0.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _ = self.successful();
    }
}

struct Before<'a>(&'a mut TcpStream, Instant);
impl Read for Before<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.1.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.0.set_read_timeout(Some(remaining))?;
        self.0.read(buffer)
    }
}

fn admit(stream: &mut TcpStream, local_key: &str) -> Pilot<()> {
    admit_route(stream, local_key, MODEL, None)
}

pub(crate) fn admit_route(
    stream: &mut TcpStream,
    local_key: &str,
    model: &str,
    review_marker: Option<&str>,
) -> Pilot<()> {
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| "GATEWAY_IO")?;
    let mut input = BufReader::new(Before(stream, Instant::now() + Duration::from_secs(3)))
        .take((LIMIT + 8192) as u64);
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|_| "GATEWAY_BAD_REQUEST")?;
    let target = line
        .trim_end()
        .strip_prefix("POST ")
        .and_then(|s| s.strip_suffix(" HTTP/1.1"))
        .ok_or("GATEWAY_BAD_REQUEST")?;
    if target.split_once('?').map_or(target, |(path, _)| path) != LOCAL_PATH {
        return Err("GATEWAY_BAD_REQUEST");
    }
    let mut length = None;
    let mut authorized = false;
    let mut keys = 0;
    let expected = format!("Bearer {local_key}");
    for _ in 0..48 {
        line.clear();
        let n = input
            .read_line(&mut line)
            .map_err(|_| "GATEWAY_BAD_REQUEST")?;
        if n == 0 || n > 4096 {
            return Err("GATEWAY_BAD_REQUEST");
        }
        if line == "\r\n" {
            break;
        }
        let (header, value) = line
            .trim_end()
            .split_once(':')
            .ok_or("GATEWAY_BAD_REQUEST")?;
        if header.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err("GATEWAY_BAD_REQUEST");
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "GATEWAY_BAD_REQUEST")?,
            );
        } else if header.eq_ignore_ascii_case("transfer-encoding") {
            return Err("GATEWAY_BAD_REQUEST");
        } else if header.eq_ignore_ascii_case("x-api-key") {
            keys += 1;
            authorized |= value.trim() == local_key;
        } else if header.eq_ignore_ascii_case("authorization") {
            keys += 1;
            authorized |= value.trim() == expected;
        }
    }
    if line != "\r\n" || keys != 1 || !authorized {
        return Err("GATEWAY_UNAUTHORIZED");
    }
    let length = length
        .filter(|n| *n > 0 && *n <= LIMIT)
        .ok_or("GATEWAY_BAD_REQUEST")?;
    let mut body = vec![0; length];
    input
        .read_exact(&mut body)
        .map_err(|_| "GATEWAY_BAD_REQUEST")?;
    let body = std::str::from_utf8(&body).map_err(|_| "GATEWAY_BAD_REQUEST")?;
    let data = relay_protocol::frame::decode_json_value(body).map_err(|_| "GATEWAY_BAD_REQUEST")?;
    let invalid_tools = data.get("tools").is_some_and(|v| {
        v.as_array().is_none_or(|a| {
            !a.is_empty() && !(review_marker.is_some() && a.len() == 1 && a[0]["name"] == "skill")
        })
    });
    let correct_prompt = review_marker.is_none_or(|marker| {
        let content = &data["messages"];
        content.as_array().is_some_and(|a| {
            a.len() == 1
                && a[0]["role"] == "user"
                && a[0]["content"].as_array().is_some_and(|b| {
                    b.len() == 1 && b[0]["type"] == "text" && b[0]["text"] == marker
                })
        })
    });
    if data["model"] != model
        || data["stream"] != true
        || invalid_tools
        || !correct_prompt
        || data
            .get("mcp_servers")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err("GATEWAY_ROUTE_MISMATCH");
    }
    // Never forward CLI headers, prompts, metadata, or user data. Provider request()
    // is a fully rebuilt, compiled-in sample and is the same bytes previewed.
    Ok(())
}

fn upstream_command(endpoint: &str, request: Option<&Value>) -> Pilot<Command> {
    let post = if request.is_some() {
        if endpoint == GO_ENDPOINT || is_minimax_endpoint(endpoint) {
            true
        } else {
            return Err("UPSTREAM_ROUTE");
        }
    } else if endpoint == GO_USAGE {
        false
    } else {
        return Err("UPSTREAM_ROUTE");
    };
    let mut command = Command::new("/usr/bin/curl");
    // curl only honors --disable as the first argument.
    command.args([
        "--disable",
        "--silent",
        "--max-time",
        if post { "25" } else { "7" },
        "--connect-timeout",
        "5",
        "--max-filesize",
        "131072",
        "--retry",
        "0",
        "--noproxy",
        "*",
        "--proto",
        "=https",
        "--config",
        "-",
        "--write-out",
        "\n%{http_code}:%{content_type}\n",
    ]);
    if post {
        command.args(["--header", "Content-Type: application/json"]);
        if endpoint == GO_ENDPOINT {
            command.args([
                "--user-agent",
                concat!("AgentRelay/", env!("CARGO_PKG_VERSION")),
                "--header",
                &format!("x-opencode-session: ses_{}", nonce()?),
            ]);
        } else {
            command.args(["--header", "anthropic-version: 2023-06-01"]);
        }
    }
    command
        .arg(endpoint)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(command)
}

// curl parses this private stdin stream as a config, not a shell command.
// Both the Authorization header and the exact JSON body stay out of argv.
fn curl_input(mut out: impl Write, key: &str, request: Option<&Value>) -> Pilot<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
    {
        return Err("UPSTREAM_KEY_FORMAT");
    }
    let encoded = request
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| "UPSTREAM_PROTOCOL")?;
    if encoded.as_ref().is_some_and(|body| body.len() > LIMIT) {
        return Err("UPSTREAM_INPUT_LIMIT");
    }
    writeln!(out, "header = \"Authorization: Bearer {key}\"").map_err(|_| "UPSTREAM_START")?;
    if let Some(body) = encoded {
        // Serialized JSON has no literal newline, tab, or CR bytes.
        let mut escaped = String::with_capacity(body.len());
        for c in body.chars() {
            if matches!(c, '\\' | '"') {
                escaped.push('\\');
            }
            escaped.push(c);
        }
        writeln!(out, "data-binary = \"{escaped}\"").map_err(|_| "UPSTREAM_START")?;
    }
    Ok(())
}

fn upstream(key: &str, endpoint: &str, request: Option<&Value>) -> Pilot<Vec<u8>> {
    upstream_scoped(ProcessScope::Standalone, key, endpoint, request)
}

pub(crate) fn upstream_scoped(
    scope: ProcessScope,
    key: &str,
    endpoint: &str,
    request: Option<&Value>,
) -> Pilot<Vec<u8>> {
    let mut configuration = Vec::new();
    let encoded = curl_input(&mut configuration, key, request);
    let write_result = (|| {
        encoded?;
        let mut cmd = upstream_command(endpoint, request)?;
        let mut child = ChildGroup(
            scope
                .apply(&mut cmd)
                .spawn()
                .map_err(|_| "UPSTREAM_START")?,
            scope,
        );
        child
            .0
            .stdin
            .take()
            .ok_or("UPSTREAM_START")?
            .write_all(&configuration)
            .map_err(|_| "UPSTREAM_START")?;
        Ok(child)
    })();
    configuration.fill(0);
    let mut child = write_result?;
    let output = crate::service::read_bounded(
        child.0.stdout.take().ok_or("UPSTREAM_START")?,
        LIMIT,
        Instant::now() + Duration::from_secs(27),
    );
    if output.timed_out || output.oversized || output.read_error || !child.successful() {
        return Err("UPSTREAM_TRANSPORT");
    }
    let bytes = std::str::from_utf8(&output.bytes).map_err(|_| "UPSTREAM_PROTOCOL")?;
    let (body, meta) = bytes
        .strip_suffix('\n')
        .and_then(|s| s.rsplit_once('\n'))
        .ok_or("UPSTREAM_PROTOCOL")?;
    let (status, content) = meta.split_once(':').ok_or("UPSTREAM_PROTOCOL")?;
    match status {
        "401" | "403" => Err("UPSTREAM_AUTH"),
        "429" => Err("UPSTREAM_RATE_LIMIT"),
        "200"
            if content.split(';').next()
                == Some(if is_minimax_endpoint(endpoint) {
                    "text/event-stream"
                } else {
                    "application/json"
                }) =>
        {
            Ok(body.as_bytes().to_vec())
        }
        _ => Err("UPSTREAM_PROTOCOL"),
    }
}

/// Retry the same compiled request on 429 / transient transport failures.
/// The sample is deterministic and already approved, so re-sending is safe;
/// auth/protocol/model failures return immediately.
pub(crate) fn upstream_with_retry(
    scope: ProcessScope,
    key: &str,
    endpoint: &str,
    request: Option<&Value>,
    max_retries: u32,
    interval: Duration,
) -> Pilot<Vec<u8>> {
    let mut retries = 0;
    loop {
        match upstream_scoped(scope, key, endpoint, request) {
            Ok(bytes) => return Ok(bytes),
            Err(code) if retryable(code) && retries < max_retries => {
                retries += 1;
                std::thread::sleep(interval);
            }
            Err(code) if retryable(code) => return Err("UPSTREAM_RETRY_EXHAUSTED"),
            Err(code) => return Err(code),
        }
    }
}

pub(crate) fn provider_evidence(bytes: &[u8], model: &str) -> Pilot<Value> {
    let text = std::str::from_utf8(bytes).map_err(|_| "UPSTREAM_PROTOCOL")?;
    let (mut started, mut end_turn, mut ended) = (false, false, false);
    let mut event = None;
    let mut usage = Value::Object(serde_json::Map::new());
    for line in text.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            if event.replace(name.trim()).is_some() {
                return Err("UPSTREAM_PROTOCOL");
            }
            continue;
        }
        let value = line.strip_prefix("data:").ok_or("UPSTREAM_PROTOCOL")?;
        let data = relay_protocol::frame::decode_json_value(value.trim())
            .map_err(|_| "UPSTREAM_PROTOCOL")?;
        let event_name = event.take().ok_or("UPSTREAM_PROTOCOL")?;
        if data["type"].as_str() != Some(event_name) {
            return Err("UPSTREAM_PROTOCOL");
        }
        let fields = match event_name {
            "ping" => continue,
            "error" => {
                return Err(match data["error"]["type"].as_str() {
                    Some("authentication_error" | "permission_error") => "UPSTREAM_AUTH",
                    Some("rate_limit_error") => "UPSTREAM_RATE_LIMIT",
                    _ => "UPSTREAM_PROVIDER_ERROR",
                })
            }
            "message_start" if !started => {
                if data["message"]["model"].as_str() != Some(model) {
                    return Err("MODEL_MISMATCH");
                }
                if data["message"]["content"]
                    .as_array()
                    .is_none_or(|a| !a.is_empty())
                {
                    return Err("UPSTREAM_PROTOCOL");
                }
                started = true;
                &data["message"]["usage"]
            }
            "content_block_start" if started && !end_turn => {
                if !matches!(
                    data["content_block"]["type"].as_str(),
                    Some("text" | "thinking" | "redacted_thinking")
                ) {
                    return Err("UNEXPECTED_TOOL_RESPONSE");
                }
                continue;
            }
            "content_block_delta" if started && !end_turn => {
                if !matches!(
                    data["delta"]["type"].as_str(),
                    Some("text_delta" | "thinking_delta" | "signature_delta")
                ) {
                    return Err("UNEXPECTED_TOOL_RESPONSE");
                }
                continue;
            }
            "content_block_stop" if started && !end_turn => continue,
            "message_delta" if started && !end_turn => {
                match data["delta"]["stop_reason"].as_str() {
                    Some("end_turn") => end_turn = true,
                    Some("tool_use") => return Err("UNEXPECTED_TOOL_RESPONSE"),
                    Some(_) => return Err("PROVIDER_INCOMPLETE"),
                    None if !data["delta"]["stop_reason"].is_null() => {
                        return Err("UPSTREAM_PROTOCOL")
                    }
                    None => {}
                }
                &data["usage"]
            }
            "message_stop" if end_turn && !ended => {
                ended = true;
                continue;
            }
            _ => return Err("UPSTREAM_PROTOCOL"),
        };
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ] {
            if let Some(n) = fields[key].as_u64() {
                usage[key] = n.into();
            }
        }
    }
    if !ended || event.is_some() {
        return Err("UPSTREAM_PROTOCOL");
    }
    Ok(usage)
}

pub(crate) struct StageSandbox<'a> {
    pub cli: &'a Path,
    pub home: &'a Path,
    pub repo: &'a Path,
    pub managed: &'a Path,
    pub scope: ProcessScope,
}

fn claude_session(
    pilot: &Path,
    excluded: &Path,
    cli: &Path,
    prompt: &str,
    send: impl FnOnce() -> Pilot<Vec<u8>>,
) -> Pilot<(String, Value)> {
    claude_scoped(
        StageSandbox {
            cli,
            home: pilot,
            repo: excluded,
            managed: excluded,
            scope: ProcessScope::Standalone,
        },
        prompt,
        send,
    )
}

pub(crate) fn claude_scoped(
    settings: StageSandbox<'_>,
    prompt: &str,
    send: impl FnOnce() -> Pilot<Vec<u8>>,
) -> Pilot<(String, Value)> {
    let StageSandbox {
        cli,
        home: pilot,
        repo,
        managed,
        scope,
    } = settings;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| "GATEWAY_BIND")?;
    let port = listener.local_addr().map_err(|_| "GATEWAY_BIND")?.port();
    listener.set_nonblocking(true).map_err(|_| "GATEWAY_BIND")?;
    let local_key = nonce()?;
    let input_path = pilot.join("sample");
    fs::write(&input_path, prompt).map_err(|_| "PILOT_TEMP")?;
    let stdin = File::open(&input_path).map_err(|_| "PILOT_TEMP")?;
    fs::remove_file(input_path).map_err(|_| "PILOT_TEMP")?;
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-D")
        .arg(format!("CLI={}", cli.display()))
        .arg("-D")
        .arg(format!("HOME={}", pilot.display()))
        .arg("-D")
        .arg(format!("REPO={}", repo.display()))
        .arg("-D")
        .arg(format!("MANAGED={}", managed.display()))
        .arg("-D")
        .arg(format!("ENDPOINT=localhost:{port}"))
        .arg("-p")
        .arg(crate::claude_review::PROFILE)
        .arg(cli)
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
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", pilot)
        .env("TMPDIR", pilot)
        .env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{port}/anthropic"),
        )
        .env("ANTHROPIC_API_KEY", &local_key)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
        .current_dir(pilot)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup(
        scope.apply(&mut command).spawn().map_err(|_| "CLI_START")?,
        scope,
    );

    let stdout = child.0.stdout.take().ok_or("CLI_START")?;
    let read = std::thread::spawn(move || {
        crate::service::read_bounded(stdout, 512 * 1024, Instant::now() + Duration::from_secs(45))
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                if matches!(child.0.try_wait(), Ok(Some(_))) {
                    return Err("CLI_EXIT_BEFORE_REQUEST");
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            _ => return Err("GATEWAY_NO_REQUEST"),
        }
    };
    // Irrevocably remove the listening socket before any live request can be made.
    drop(listener);
    admit(&mut stream, &local_key)?;
    let data = send()?;
    let provider_usage = provider_evidence(&data, MODEL)?;
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len())
        .and_then(|()| stream.write_all(&data)).map_err(|_| "GATEWAY_IO")?;
    drop(stream);
    let evidence = read.join().map_err(|_| "CLI_OUTPUT")?;
    if evidence.timed_out || evidence.oversized || evidence.read_error || !child.successful() {
        return Err("CLI_OUTPUT");
    }
    let output = crate::claude_wire::validate(evidence.bytes.as_slice(), MODEL)?;
    Ok((output, provider_usage))
}

fn sample(pilot: &Path, cli: &Path, send: impl FnOnce() -> Pilot<Vec<u8>>) -> Pilot<Value> {
    let (output, provider_usage) = claude_session(pilot, pilot, cli, SAMPLE, send)?;
    if output.trim() != "relay-pilot-ok" {
        return Err("SAMPLE_UNEXPECTED");
    }
    Ok(
        json!({"actual_model":MODEL,"terminal":"end_turn","requests_for_sample":1,
        "charge_attribution":"unverified","provider_usage":provider_usage,"usage_source":"provider_messages_sse","production_enabled":false}),
    )
}

fn pilot_git(repo: &Path, arguments: &[&str]) -> Pilot<String> {
    let output = Command::new("/usr/bin/git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.templateDir=",
        ])
        .args(arguments)
        .current_dir(repo)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Relay Sample")
        .env("GIT_AUTHOR_EMAIL", "sample@example.invalid")
        .env("GIT_COMMITTER_NAME", "Relay Sample")
        .env("GIT_COMMITTER_EMAIL", "sample@example.invalid")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "SAMPLE_GIT")?;
    if !output.status.success() {
        return Err("SAMPLE_GIT");
    }
    String::from_utf8(output.stdout).map_err(|_| "SAMPLE_GIT")
}

struct VerifiedEdit {
    evidence: Value,
    baseline: PathBuf,
    worktree: PathBuf,
    base: String,
    scope: relay_git::WriteScope,
    patch: String,
    receipt: String,
}

impl VerifiedEdit {
    fn review_payload(&self) -> Pilot<Value> {
        crate::route_payload::review_candidate(EDIT_GOAL, &self.patch, &self.receipt)
    }

    fn unchanged(&self) -> Pilot<()> {
        if !pilot_git(
            &self.baseline,
            &["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty()
            || pilot_git(&self.baseline, &["rev-parse", "HEAD"])?.trim() != self.base
            || relay_git::collect_diff(&self.worktree, &self.base, &self.scope)
                .map_err(|_| "SAMPLE_DIFF")?
                .patch
                != self.patch
        {
            return Err("SAMPLE_CHANGED_DURING_REVIEW");
        }
        Ok(())
    }
}

fn edit_sample(
    root: &Path,
    cli: &Path,
    send: impl FnOnce() -> Pilot<Vec<u8>>,
) -> Pilot<VerifiedEdit> {
    let repo = root.join("baseline");
    let managed = root.join("managed");
    let home = root.join("cli-home");
    for dir in [&repo, &managed, &home] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .map_err(|_| "PILOT_TEMP")?;
    }
    pilot_git(&repo, &["init", "--quiet"])?;
    fs::write(repo.join(EDIT_FILE), EDIT_BEFORE).map_err(|_| "SAMPLE_GIT")?;
    pilot_git(&repo, &["add", "--", EDIT_FILE])?;
    pilot_git(&repo, &["commit", "--quiet", "-m", "synthetic baseline"])?;
    let base = pilot_git(&repo, &["rev-parse", "HEAD"])?;
    let base = base.trim();
    let worktree = relay_git::create_worktree(&repo, &managed, "task_pilot", 1, base)
        .map_err(|_| "SAMPLE_WORKTREE")?;
    let scope = relay_git::WriteScope::new(&[EDIT_FILE]).map_err(|_| "SAMPLE_SCOPE")?;
    let rule = "sample_exact_bytes".to_owned();
    let plan = relay_store::PlanRecord {
        plan_id: "sample".into(),
        project_id: "sample".into(),
        goal: EDIT_GOAL.into(),
        brief: String::new(),
        base_oid: base.into(),
        created_at: String::new(),
        expires_at: String::new(),
        status: "unused".into(),
        write_scope: vec![EDIT_FILE.into()],
        rule_ids: vec![rule.clone()],
        profile_id: "sample".into(),
    };
    let edit = crate::managed_edits::EditRequest::new(&worktree.path, &plan, &scope, None)
        .map_err(|_| "SAMPLE_EDIT_INPUT")?;
    let expected = root.join("expected");
    fs::write(&expected, EDIT_AFTER).map_err(|_| "PILOT_TEMP")?;
    // The CLI cannot read the baseline, worktree or verifier oracle; only
    // the loopback gateway receives the exact, sealed synthetic prompt.
    let (output, provider_usage) = claude_session(&home, root, cli, &edit_prompt(), send)?;
    let replacement = relay_protocol::frame::decode_json_value(output.trim())
        .map_err(|_| "SAMPLE_EDIT_SCHEMA")?;
    edit.apply(&replacement)
        .map_err(|_| "SAMPLE_EDIT_REJECTED")?;
    let diff = relay_git::collect_diff(&worktree.path, base, &scope).map_err(|_| "SAMPLE_DIFF")?;
    if diff.changes.len() != 1
        || diff.changes[0].path != EDIT_FILE
        || diff.changes[0].action != relay_git::ChangeAction::Modified
    {
        return Err("SAMPLE_DIFF");
    }
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: rule.clone(),
        executable: "/usr/bin/cmp".into(),
        argv: vec![
            "-s".into(),
            EDIT_FILE.into(),
            expected.to_string_lossy().into_owned(),
        ],
        timeout_ms: 1_000,
    }])
    .map_err(|_| "SAMPLE_VERIFY")?;
    let receipt = relay_git::run_and_build_receipt(
        &config,
        &[rule],
        &worktree.path,
        base,
        "task_pilot/1",
        &diff,
        &scope,
        &crate::service::now_rfc3339(),
    )
    .map_err(|_| "SAMPLE_VERIFY")?;
    let diff_sha256 = relay_git::sha256_hex(diff.patch.as_bytes());
    if receipt.status != "pass"
        || receipt.diff_sha256.as_deref() != Some(&diff_sha256)
        || !pilot_git(&repo, &["status", "--porcelain", "--untracked-files=all"])?.is_empty()
        || pilot_git(&repo, &["rev-parse", "HEAD"])?.trim() != base
    {
        return Err("SAMPLE_VERIFY");
    }
    Ok(VerifiedEdit {
        evidence: json!({"actual_model":MODEL,"terminal":"end_turn","requests_for_sample":1,
            "charge_attribution":"unverified","provider_usage":provider_usage,
            "usage_source":"provider_messages_sse","changed_file":EDIT_FILE,
            "diff_sha256":diff_sha256,"receipt_sha256":relay_git::sha256_hex(receipt.json.as_bytes()),
            "verified":true,"baseline_unchanged":true,"production_enabled":false}),
        baseline: repo,
        worktree: worktree.path,
        base: base.into(),
        scope,
        patch: diff.patch,
        receipt: receipt.json,
    })
}

fn edit_fixture() -> Vec<u8> {
    fixture_for(
        MODEL,
        &json!({"schema_version":1,"edits":[{"path":EDIT_FILE,
        "before_sha256":relay_git::sha256_hex(EDIT_BEFORE.as_bytes()),"text":EDIT_AFTER}]})
        .to_string(),
    )
}

fn fixture() -> Vec<u8> {
    fixture_for(MODEL, "relay-pilot-ok")
}

pub(crate) fn fixture_for(model: &str, result: &str) -> Vec<u8> {
    let events = [
        json!({"type":"message_start","message":{"id":"msg_relay_pilot","type":"message","role":"assistant","content":[],"model":model,"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":result}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    events
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {e}\n\n",
                e["type"].as_str().expect("fixture type")
            )
        })
        .collect::<String>()
        .into_bytes()
}

fn review_result() -> Value {
    json!({"sample":"relay-pilot-review-ok","verdict":"pass"})
}

fn review_sample(
    tmp: &Path,
    cli: &Path,
    go: bool,
    send: impl FnOnce() -> Pilot<Vec<u8>>,
) -> Pilot<Value> {
    let model = if go {
        go_model()?
    } else {
        REVIEW_MODEL.to_owned()
    };
    let mut evidence = None;
    let report = crate::opencode_review::pilot_session(cli, tmp, &model, || {
        let bytes = send()?;
        if go {
            let (result, report) = go_response(&bytes, &model)?;
            if relay_protocol::frame::decode_json_value(result.trim()).ok() != Some(review_result())
            {
                return Err("SAMPLE_UNEXPECTED");
            }
            evidence = Some(report);
            // The isolated harness speaks Messages; forward only the vetted sample
            // content. Provider evidence below always comes from the original JSON.
            return Ok(fixture_for(&model, result.as_str()));
        }
        let usage = provider_evidence(&bytes, &model)?;
        evidence = Some(json!({"actual_model":&model,"terminal":"end_turn",
            "charge_attribution":"unverified","provider_usage":usage,
            "usage_source":"provider_messages_sse"}));
        Ok(bytes)
    })?;
    if report != review_result() {
        return Err("SAMPLE_UNEXPECTED");
    }
    let mut evidence = evidence.ok_or("UPSTREAM_PROTOCOL")?;
    evidence["requests_for_sample"] = 1.into();
    evidence["production_enabled"] = false.into();
    Ok(evidence)
}

pub(crate) fn go_meters(bytes: &[u8]) -> Pilot<Value> {
    let body = std::str::from_utf8(bytes).map_err(|_| "GO_USAGE_PROTOCOL")?;
    let response =
        relay_protocol::frame::decode_json_value(body).map_err(|_| "GO_USAGE_PROTOCOL")?;
    let mut report = json!({});
    for meter in ["rolling", "weekly", "monthly"] {
        let row = &response["usage"][meter];
        let percent = row["percent"]
            .as_u64()
            .filter(|p| *p <= 100)
            .ok_or("GO_USAGE_PROTOCOL")?;
        match row["status"].as_str() {
            Some("ok") if percent < 100 => report[meter] = json!({"percent":percent}),
            Some("rate-limited") => return Err("GO_QUOTA_EXHAUSTED"),
            _ => return Err("GO_USAGE_PROTOCOL"),
        }
    }
    Ok(report)
}

pub(crate) fn go_response(bytes: &[u8], expected_model: &str) -> Pilot<(String, Value)> {
    let body = std::str::from_utf8(bytes).map_err(|_| "UPSTREAM_PROTOCOL")?;
    let response =
        relay_protocol::frame::decode_json_value(body).map_err(|_| "UPSTREAM_PROTOCOL")?;
    let model = response["model"].as_str().ok_or("MODEL_MISMATCH")?;
    if model != expected_model {
        return Err("MODEL_MISMATCH");
    }
    let choices = response["choices"].as_array().ok_or("UPSTREAM_PROTOCOL")?;
    let [choice] = choices.as_slice() else {
        return Err("UPSTREAM_PROTOCOL");
    };
    let message = &choice["message"];
    if !message["tool_calls"].is_null()
        || !message["function_call"].is_null()
        || choice["finish_reason"] == "tool_calls"
    {
        return Err("UNEXPECTED_TOOL_RESPONSE");
    }
    if choice["finish_reason"] != "stop" {
        return Err("PROVIDER_INCOMPLETE");
    }
    if message["role"] != "assistant" {
        return Err("UPSTREAM_PROTOCOL");
    }
    let content = message["content"].as_str().ok_or("UPSTREAM_PROTOCOL")?;
    let usage = &response["usage"];
    let details = &usage["opencode"];
    let source = details["billing_source"]
        .as_str()
        .ok_or("GO_BILLING_UNVERIFIED")?;
    if !matches!(source, "go" | "go-plus") || usage["cost"].as_f64() != Some(0.0) {
        return Err("GO_BILLING_NOT_INCLUDED");
    }
    let allowance = details["allowance"]
        .as_f64()
        .filter(|n| *n >= 0.0 && n.is_finite())
        .ok_or("GO_BILLING_UNVERIFIED")?;
    let input = usage["prompt_tokens"]
        .as_u64()
        .ok_or("GO_BILLING_UNVERIFIED")?;
    let output = usage["completion_tokens"]
        .as_u64()
        .ok_or("GO_BILLING_UNVERIFIED")?;
    Ok((
        content.into(),
        json!({"actual_model":model,"terminal":"stop",
        "charge_attribution":{"provider_reported_source":source,"balance_charge_usd":0,"allowance_usd":allowance},
        "provider_usage":{"input_tokens":input,"output_tokens":output},
        "usage_source":"provider_chat_completions_json"}),
    ))
}

fn go_fixture_for(model: &str, result: &Value) -> Value {
    json!({"model":model,
        "choices":[{"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}],
        "usage":{"prompt_tokens":30,"completion_tokens":12,"cost":0,
        "opencode":{"billing_source":"go","allowance":0.00001}}})
}

const PAIR_REVIEW_PROMPT: &str = "Review only the supplied synthetic text change and its test receipt. Return one JSON object with schema_version 1, verdict (pass or changes_requested), candidate_diff_sha256, test_receipt_sha256 and findings. Repeat both digests unchanged. Use pass only if the patch changes status=waiting to status=ready, preserves the final newline and the exact-byte test passed. If requesting changes, include a finding with severity, evidence and suggestion. Do not run tools.";
const PAIR_CONTENT_LIMIT: usize = 8 * 1024;

fn pair_review_request(model: &str, payload: &Value) -> Value {
    let mut body = review_request(model, true);
    body["messages"][0]["content"] = format!("{PAIR_REVIEW_PROMPT} Data: {payload}").into();
    body
}

fn pair_preview(review_model: &str, claude: &Path, opencode: &Path) -> Pilot<Value> {
    let implementation = plan(claude, false, false, true)?;
    let review = plan(opencode, true, true, false)?;
    let details = json!({
        "implementation":implementation["details"],
        "review":{
            "tool":"opencode","model":review_model,"endpoint":GO_ENDPOINT,"quota_endpoint":GO_USAGE,
            "cli":review["details"]["cli"],"cli_sha256":review["details"]["cli_sha256"],
            "prompt":PAIR_REVIEW_PROMPT,"payload_source":"synthetic diff and test status; full receipt stays local",
            "payload_fields":["goal","diff","receipt","candidate_diff_sha256","test_receipt_sha256"],
            "request_limit_bytes":PAIR_CONTENT_LIMIT,
            "max_tokens":512,"reasoning_effort":"low","stream":false,
            "billing":review["details"]["billing"],
        },
        "sample":"bounded_file_edit_and_independent_review",
        "max_model_requests":2,"max_quota_reads":2,"fallback":false,
        "required_confirmation":["allow-plan-credits","extra-usage-off"],
        "production_enabled":false,
    });
    let bytes = serde_json::to_vec(&details).map_err(|_| "PLAN_ENCODING")?;
    Ok(json!({"details":details,"approval":hex(Sha256::digest(bytes))}))
}

fn audit_pair(
    root: &Path,
    claude: &Path,
    opencode: &Path,
    review_model: &str,
    implement: impl FnOnce() -> Pilot<Vec<u8>>,
    review: impl FnOnce(&Value, &Value) -> Pilot<Vec<u8>>,
) -> Pilot<Value> {
    let edit = edit_sample(root, claude, implement)?;
    let payload = edit.review_payload()?;
    let request = pair_review_request(review_model, &payload);
    if request.to_string().len() > PAIR_CONTENT_LIMIT {
        return Err("PAIR_REVIEW_INPUT_LIMIT");
    }
    let home = root.join("opencode-home");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .map_err(|_| "PILOT_TEMP")?;
    let mut evidence = None;
    let report = crate::opencode_review::pilot_review_candidate(
        opencode,
        &home,
        root,
        review_model,
        || {
            let data = review(&request, &payload)?;
            let (content, upstream) = go_response(&data, review_model)?;
            if content.len() > 16 * 1024
                || relay_protocol::frame::decode_json_value(content.trim()).is_err()
            {
                return Err("PAIR_REVIEW_PROTOCOL");
            }
            evidence = Some(upstream);
            Ok(fixture_for(review_model, &content))
        },
    )?;
    edit.unchanged()?;
    let diff = edit.evidence["diff_sha256"]
        .as_str()
        .ok_or("SAMPLE_VERIFY")?;
    let receipt = edit.evidence["receipt_sha256"]
        .as_str()
        .ok_or("SAMPLE_VERIFY")?;
    let validated = crate::workflow::evaluate_review_report(&report, diff, receipt)
        .map_err(|_| "PAIR_REVIEW_INVALID_OR_STALE")?;
    if validated.verdict != crate::workflow::ReviewVerdict::Pass {
        return Err("PAIR_REVIEW_CHANGES_REQUESTED");
    }
    Ok(json!({
        "implementation":edit.evidence,
        "review":evidence.ok_or("PAIR_REVIEW_PROTOCOL")?,
        "verdict":"pass","diff_sha256":diff,"test_receipt_sha256":receipt,
        "review_report_sha256":relay_git::sha256_hex(report.to_string().as_bytes()),
        "sample_verified":true,"baseline_unchanged":true,"production_enabled":false
    }))
}

fn pair_entry(action: &str, claude_bin: &str, args: &[String]) -> Pilot<Value> {
    let [opencode_bin, rest @ ..] = args else {
        return Err("USAGE");
    };
    let canonical = |bin: &str| {
        Path::new(bin)
            .canonicalize()
            .map_err(|_| "CLI_NOT_READABLE")
    };
    let claude = canonical(claude_bin)?;
    let opencode = canonical(opencode_bin)?;
    if claude == opencode {
        return Err("PAIR_DISTINCT_CLIS_REQUIRED");
    }
    let review_model = go_model()?;
    let preview = pair_preview(&review_model, &claude, &opencode)?;
    match (action, rest) {
        ("preview", []) => Ok(preview),
        ("offline-check", [] | [_]) => {
            let stale = match rest {
                [] => None,
                [flag] if flag == "--stale-diff" => Some("candidate_diff_sha256"),
                [flag] if flag == "--stale-receipt" => Some("test_receipt_sha256"),
                _ => return Err("USAGE"),
            };
            let tmp = Temp::new()?;
            let evidence = audit_pair(
                &tmp.0,
                &claude,
                &opencode,
                &review_model,
                || Ok(edit_fixture()),
                |_, payload| {
                    let mut report = json!({
                        "schema_version":1,"verdict":"pass","findings":[],
                        "candidate_diff_sha256":payload["candidate_diff_sha256"],
                        "test_receipt_sha256":payload["test_receipt_sha256"]
                    });
                    if let Some(field) = stale {
                        report[field] = "0".repeat(64).into();
                    }
                    Ok(go_fixture_for(&review_model, &report)
                        .to_string()
                        .into_bytes())
                },
            )?;
            Ok(json!({"mode":"synthetic_pair","external_requests":0,"evidence":evidence}))
        }
        ("run", [flag, approval, credits, extra])
            if flag == "--approve"
                && preview["approval"] == approval.as_str()
                && credits == "--allow-plan-credits"
                && extra == "--extra-usage-off" =>
        {
            let mut input = read_piped_keys(516)?;
            let result = (|| {
                let bytes = input.strip_suffix(b"\n").unwrap_or(&input);
                let mut lines = bytes.split(|b| *b == b'\n');
                let (Some(claude_key), Some(go_key), None) =
                    (lines.next(), lines.next(), lines.next())
                else {
                    return Err("PAIR_REQUIRES_TWO_KEYS");
                };
                let claude_key = subscription_key(claude_key, false)?;
                let go_key = subscription_key(go_key, true)?;
                if claude_key == go_key || go_key.starts_with("sk-cp-") {
                    return Err("PAIR_REQUIRES_TWO_KEYS");
                }
                let endpoint = minimax_endpoint()?;
                // Verify that the second stage is eligible before the first can spend,
                // and refresh just before the actual Go request.
                go_meters(&upstream(go_key, GO_USAGE, None)?)?;
                let tmp = Temp::new()?;
                audit_pair(
                    &tmp.0,
                    &claude,
                    &opencode,
                    &review_model,
                    || upstream(claude_key, endpoint, Some(&edit_request())),
                    |request, _| {
                        go_meters(&upstream(go_key, GO_USAGE, None)?)?;
                        upstream(go_key, GO_ENDPOINT, Some(request))
                    },
                )
            })();
            input.fill(0);
            Ok(json!({"mode":"live_pair","approval":preview["approval"],"evidence":result?}))
        }
        ("run", _) => Err("APPROVAL_REQUIRED"),
        _ => Err("USAGE"),
    }
}

fn read_key() -> Pilot<Vec<u8>> {
    read_piped_keys(257)
}

pub(crate) fn read_piped_keys(limit: u64) -> Pilot<Vec<u8>> {
    if io::stdin().is_terminal() {
        return Err("SUBSCRIPTION_KEY_REQUIRES_PIPE");
    }
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(limit + 1)
        .read_to_end(&mut input)
        .map_err(|_| "SUBSCRIPTION_KEY_REQUIRED")?;
    if input.len() as u64 > limit {
        input.fill(0);
        return Err("SUBSCRIPTION_KEY_REQUIRED");
    }
    Ok(input)
}

pub(crate) fn subscription_key(input: &[u8], go: bool) -> Pilot<&str> {
    let key = std::str::from_utf8(input)
        .map_err(|_| "SUBSCRIPTION_KEY_REQUIRED")?
        .trim_end_matches(['\n', '\r']);
    if (!go && !key.starts_with("sk-cp-"))
        || !(16..=256).contains(&key.len())
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("SUBSCRIPTION_KEY_REQUIRED");
    }
    Ok(key)
}

fn entry(args: &[String]) -> Pilot<Value> {
    let [action, bin, rest @ ..] = args else {
        return Err("USAGE");
    };
    if let Some(pair_action) = action.strip_prefix("pair-") {
        return pair_entry(pair_action, bin, rest);
    }
    if action == "verify-sse" && rest.is_empty() {
        let mut bytes = Vec::new();
        io::stdin()
            .lock()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "UPSTREAM_PROTOCOL")?;
        if bytes.len() > LIMIT || bin.is_empty() || bin.len() > 128 {
            return Err("UPSTREAM_PROTOCOL");
        }
        return Ok(json!({"actual_model":bin, "provider_usage":provider_evidence(&bytes, bin)?}));
    }
    let cli = Path::new(bin)
        .canonicalize()
        .map_err(|_| "CLI_NOT_READABLE")?;
    let go = action.starts_with("go-");
    let edit = action.starts_with("edit-");
    let method = action.strip_prefix("edit-").unwrap_or(action);
    let (review, action) = method
        .strip_prefix("go-")
        .or_else(|| method.strip_prefix("review-"))
        .map_or((false, method), |a| (true, a));
    if edit && review {
        return Err("USAGE");
    }
    let preview = plan(&cli, review, go, edit)?;
    match (action, rest) {
        ("preview", []) => Ok(preview),
        ("offline-check", []) => {
            let tmp = Temp::new()?;
            let evidence = if review {
                review_sample(&tmp.0, &cli, go, || {
                    if go {
                        Ok(go_fixture_for(&go_model()?, &review_result())
                            .to_string()
                            .into_bytes())
                    } else {
                        Ok(fixture_for(REVIEW_MODEL, &review_result().to_string()))
                    }
                })?
            } else if edit {
                edit_sample(&tmp.0, &cli, || Ok(edit_fixture()))?.evidence
            } else {
                sample(&tmp.0, &cli, || Ok(fixture()))?
            };
            Ok(json!({"mode":"synthetic","evidence":evidence,"external_requests":0}))
        }
        ("quota", []) if go => {
            let mut input = read_key()?;
            let result = subscription_key(&input, true)
                .and_then(|key| upstream(key, GO_USAGE, None))
                .and_then(|bytes| go_meters(&bytes));
            input.fill(0);
            Ok(json!({"mode":"quota_read_only", "provider_requests":0, "usage":result?}))
        }
        ("run", [flag, approval, options @ ..])
            if flag == "--approve"
                && preview["approval"] == approval.as_str()
                && ((go && options == ["--extra-usage-off"])
                    || (!go && options == ["--allow-plan-credits"])) =>
        {
            let mut input = read_key()?;
            let result = (|| {
                let key = subscription_key(&input, go)?;
                let tmp = Temp::new()?;
                if go {
                    let review_model = go_model()?;
                    go_meters(&upstream(key, GO_USAGE, None)?)?;
                    review_sample(&tmp.0, &cli, true, || {
                        upstream(key, GO_ENDPOINT, Some(&review_request(&review_model, true)))
                    })
                } else if review {
                    let endpoint = minimax_endpoint()?;
                    review_sample(&tmp.0, &cli, false, || {
                        upstream(key, endpoint, Some(&review_request(REVIEW_MODEL, false)))
                    })
                } else if edit {
                    let endpoint = minimax_endpoint()?;
                    edit_sample(&tmp.0, &cli, || {
                        upstream(key, endpoint, Some(&edit_request()))
                    })
                    .map(|edit| edit.evidence)
                } else {
                    let endpoint = minimax_endpoint()?;
                    sample(&tmp.0, &cli, || upstream(key, endpoint, Some(&request())))
                }
            })();
            input.fill(0);
            Ok(
                json!({"mode":"live_pilot","tool":preview["details"]["tool"],"role":preview["details"]["role"],
                "approval":preview["approval"],"evidence":result?}),
            )
        }
        ("run", _) => Err("APPROVAL_REQUIRED"),
        _ => Err("USAGE"),
    }
}

pub fn run(args: &[String]) -> Result<(), u8> {
    match entry(args) {
        Ok(report) => {
            println!("{report}");
            Ok(())
        }
        Err(code) => {
            eprintln!("route-pilot: {code}");
            if code == "USAGE" {
                eprintln!("route-pilot preview|offline-check <Claude CLI>; route-pilot run <Claude CLI> --approve <approval> --allow-plan-credits");
                eprintln!("route-pilot pair-preview|pair-offline-check <Claude CLI> <OpenCode CLI>; route-pilot pair-run <Claude CLI> <OpenCode CLI> --approve <approval> --allow-plan-credits --extra-usage-off");
                eprintln!("route-pilot edit-preview|edit-offline-check <Claude CLI>; route-pilot edit-run <Claude CLI> --approve <approval> --allow-plan-credits");
                eprintln!("route-pilot go-preview|go-offline-check|go-quota <OpenCode CLI>; route-pilot go-run <OpenCode CLI> --approve <approval> --extra-usage-off");
                eprintln!("route-pilot review-preview|review-offline-check <OpenCode CLI>; route-pilot review-run <OpenCode CLI> --approve <approval>");
            }
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_uses_one_pinned_https_endpoint_and_a_real_post_body() {
        for (endpoint, body) in [
            (ENDPOINT, Some(edit_request())),
            (CN_ENDPOINT, Some(edit_request())),
            (GO_ENDPOINT, Some(review_request(GO_MODEL, true))),
            (GO_USAGE, None),
        ] {
            let cmd = upstream_command(endpoint, body.as_ref()).unwrap();
            let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
            assert_eq!(args[0], "--disable");
            assert_eq!(args.last(), Some(&endpoint));
            assert!(!args.contains(&"--location"));
            assert!(args.windows(2).any(|a| a == ["--config", "-"]));
            assert!(!args
                .windows(2)
                .any(|a| a[0] == "--header" && a[1].starts_with("--")));
            assert!(!args.contains(&"--data-binary"));
            assert!(!args.iter().any(|arg| arg.contains(EDIT_BEFORE)));
        }
        assert!(upstream_command(GO_USAGE, Some(&edit_request())).is_err());
        assert!(upstream_command("https://example.invalid", Some(&edit_request())).is_err());
    }

    #[test]
    fn minimax_endpoint_defaults_international_and_accepts_cn_override() {
        assert_eq!(minimax_endpoint_value(None), Ok(ENDPOINT));
        assert_eq!(minimax_endpoint_value(Some(ENDPOINT)), Ok(ENDPOINT));
        assert_eq!(minimax_endpoint_value(Some(CN_ENDPOINT)), Ok(CN_ENDPOINT));
        assert_eq!(
            minimax_endpoint_value(Some("https://example.invalid/v1/messages")),
            Err("MINIMAX_ENDPOINT_INVALID")
        );
        assert!(is_minimax_endpoint(ENDPOINT));
        assert!(is_minimax_endpoint(CN_ENDPOINT));
        assert!(!is_minimax_endpoint(GO_ENDPOINT));
    }

    #[test]
    fn go_model_defaults_and_rejects_invalid_overrides() {
        assert_eq!(go_model_value(None), Ok(GO_MODEL.to_owned()));
        assert_eq!(
            go_model_value(Some("zhipuai/glm-5.3-flash")),
            Ok("zhipuai/glm-5.3-flash".to_owned())
        );
        assert_eq!(go_model_value(Some("")), Err("GO_MODEL_INVALID"));
        assert_eq!(go_model_value(Some("bad model")), Err("GO_MODEL_INVALID"));
        assert_eq!(go_model_value(Some(MODEL)), Err("GO_MODEL_INVALID"));
        assert_eq!(
            go_model_value(Some(&"a".repeat(129))),
            Err("GO_MODEL_INVALID")
        );
    }

    #[test]
    fn retry_and_fallback_settings_are_bounded() {
        assert_eq!(
            retry_settings(None, None),
            Ok((
                DEFAULT_MAX_RETRIES,
                Duration::from_millis(DEFAULT_RETRY_INTERVAL_MS)
            ))
        );
        assert_eq!(
            retry_settings(Some("3"), Some("1500")),
            Ok((3, Duration::from_millis(1500)))
        );
        assert_eq!(
            retry_settings(Some("21"), None),
            Err("RETRY_SETTINGS_INVALID")
        );
        assert_eq!(
            retry_settings(Some("x"), None),
            Err("RETRY_SETTINGS_INVALID")
        );
        assert_eq!(
            retry_settings(None, Some("500")),
            Err("RETRY_SETTINGS_INVALID")
        );
        assert!(retryable("UPSTREAM_RATE_LIMIT"));
        assert!(retryable("UPSTREAM_TRANSPORT"));
        assert!(!retryable("UPSTREAM_AUTH"));
        assert!(!retryable("UPSTREAM_PROTOCOL"));
        assert_eq!(fallback_model_value(None, "primary"), Ok(None));
        assert_eq!(fallback_model_value(Some(""), "primary"), Ok(None));
        assert_eq!(
            fallback_model_value(Some("alt-model"), "primary"),
            Ok(Some("alt-model".to_owned()))
        );
        assert_eq!(
            fallback_model_value(Some("primary"), "primary"),
            Err("FALLBACK_MODEL_INVALID")
        );
        assert_eq!(
            fallback_model_value(Some(MODEL), "primary"),
            Err("FALLBACK_MODEL_INVALID")
        );
        assert_eq!(
            fallback_model_value(Some("bad model"), "primary"),
            Err("FALLBACK_MODEL_INVALID")
        );
    }

    #[test]
    fn curl_reads_the_exact_body_and_authorization_from_private_stdin() {
        // Test curl's own config parser against a loopback socket; production
        // still pins HTTPS and uses only the provider URLs in upstream_command.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let body = json!({"file":"a path \"quoted\"\\line\r\n\t保留","shellish":"$(touch x)"});
        let expected = body.to_string();
        let mut child = Command::new("/usr/bin/curl")
            .args([
                "--disable",
                "--silent",
                "--max-time",
                "3",
                "--noproxy",
                "*",
                "--config",
                "-",
                &endpoint,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        curl_input(child.stdin.take().unwrap(), "fixture-key", Some(&body)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("curl did not send the POST: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("POST / HTTP/"));
        let mut auth = false;
        let mut count = None;
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (key, value) = line.trim_end().split_once(':').unwrap();
            if key.eq_ignore_ascii_case("authorization") {
                auth = value.trim() == "Bearer fixture-key";
            } else if key.eq_ignore_ascii_case("content-length") {
                count = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        assert!(auth);
        let mut actual = vec![0; count.unwrap()];
        reader.read_exact(&mut actual).unwrap();
        assert_eq!(actual, expected.as_bytes());
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        assert!(child.wait().unwrap().success());

        let mut unused = Vec::new();
        assert_eq!(
            curl_input(&mut unused, "bad\r\nurl=https://invalid", None),
            Err("UPSTREAM_KEY_FORMAT")
        );
        assert!(unused.is_empty());
    }

    #[test]
    fn go_requires_quota_and_provider_model_terminal_and_billing_evidence() {
        let mut usage = json!({"usage":{
            "rolling":{"percent":40,"status":"ok"},"weekly":{"percent":65,"status":"ok"},
            "monthly":{"percent":70,"status":"ok"}}});
        assert_eq!(
            go_meters(usage.to_string().as_bytes()).unwrap()["weekly"]["percent"],
            65
        );
        usage["usage"]["weekly"] = json!({"percent":100,"status":"rate-limited"});
        assert_eq!(
            go_meters(usage.to_string().as_bytes()),
            Err("GO_QUOTA_EXHAUSTED")
        );
        assert_eq!(go_meters(b"{}"), Err("GO_USAGE_PROTOCOL"));
        assert_eq!(
            go_response(
                go_fixture_for(GO_MODEL, &review_result())
                    .to_string()
                    .as_bytes(),
                GO_MODEL,
            )
            .unwrap()
            .1["charge_attribution"]["balance_charge_usd"],
            0
        );
        for (fields, change, failure) in [
            (&["model"][..], json!("other-model"), "MODEL_MISMATCH"),
            (
                &["choices", "0", "finish_reason"],
                json!("length"),
                "PROVIDER_INCOMPLETE",
            ),
            (
                &["choices", "0", "message", "tool_calls"],
                json!([{}]),
                "UNEXPECTED_TOOL_RESPONSE",
            ),
            (
                &["usage", "opencode", "billing_source"],
                json!("balance"),
                "GO_BILLING_NOT_INCLUDED",
            ),
            (
                &["usage", "opencode", "billing_source"],
                Value::Null,
                "GO_BILLING_UNVERIFIED",
            ),
            (&["usage", "cost"], json!(0.005), "GO_BILLING_NOT_INCLUDED"),
        ] {
            let mut fixture = go_fixture_for(GO_MODEL, &review_result());
            let path = format!("/{}", fields.join("/"));
            if let Some(value) = fixture.pointer_mut(&path) {
                *value = change;
            } else {
                fixture["choices"][0]["message"]["tool_calls"] = change;
            }
            assert_eq!(
                go_response(fixture.to_string().as_bytes(), GO_MODEL),
                Err(failure),
                "{path}"
            );
        }
    }

    #[test]
    fn provider_identity_and_terminal_are_required_before_forward() {
        let bytes = fixture();
        assert_eq!(
            provider_evidence(&bytes, MODEL).unwrap(),
            json!({"input_tokens":10,"output_tokens":4})
        );
        let wrong = String::from_utf8(bytes.clone())
            .unwrap()
            .replace(MODEL, "unapproved-model");
        assert_eq!(
            provider_evidence(wrong.as_bytes(), MODEL),
            Err("MODEL_MISMATCH")
        );
        assert_eq!(
            provider_evidence(&bytes[..80], MODEL),
            Err("UPSTREAM_PROTOCOL")
        );
    }

    #[test]
    fn gateway_denies_unapproved_model_tools_or_key() {
        fn one(body: Value, usable_key: bool) -> Pilot<()> {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let mut stream = listener.accept().unwrap().0;
            let body = body.to_string();
            let key = if usable_key { "dummy" } else { "unauthorized" };
            write!(client, "POST {LOCAL_PATH}?beta=true HTTP/1.1\r\nx-api-key: {key}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            admit(&mut stream, "dummy")
        }
        assert_eq!(one(request(), true), Ok(()));
        assert_eq!(one(request(), false), Err("GATEWAY_UNAUTHORIZED"));
        let mut wrong = request();
        wrong["model"] = "different".into();
        assert_eq!(one(wrong, true), Err("GATEWAY_ROUTE_MISMATCH"));
        let mut tools = request();
        tools["tools"] = json!([{"name":"shell"}]);
        assert_eq!(one(tools, true), Err("GATEWAY_ROUTE_MISMATCH"));
    }
}
