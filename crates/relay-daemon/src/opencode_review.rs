//! Isolated OpenCode session. Daemon review uses only a synthetic loopback fixture.
use crate::service::AdapterProbeReport;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const MODEL: &str = "relay-opencode-review-model";
const MARKER: &str = "Perform the read-only review supplied by the Relay bridge.";
const EDIT_MARKER: &str = "Return the bounded file replacements supplied by the Relay bridge.";
const MAX: usize = 128 * 1024;
type Checked<T> = Result<T, &'static str>;

#[derive(Clone, Debug)]
pub struct OpenCodeReview {
    pub(crate) executable: PathBuf,
    provider_port: u16,
}

impl OpenCodeReview {
    pub(crate) fn from_config(value: &Value, field: &str) -> Result<Option<Self>, String> {
        let Some(config) = value.get(field) else {
            return Ok(None);
        };
        if !cfg!(target_os = "macos") || config.as_object().is_none_or(|v| v.len() != 2) {
            return Err("OpenCode 夹具只支持 macOS 与 executable、port 两个字段".into());
        }
        let path = PathBuf::from(
            config["executable"]
                .as_str()
                .ok_or("缺少 OpenCode executable")?,
        );
        if !path.is_file() || path.canonicalize().ok().as_ref() != Some(&path) {
            return Err("OpenCode 必须是普通文件的物理绝对路径".into());
        }
        let provider_port = config["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 1023)
            .ok_or("缺少本机非特权夹具端口")?;
        Ok(Some(Self {
            executable: path,
            provider_port,
        }))
    }

    pub(crate) fn registration(&self, implement: bool) -> AdapterProbeReport {
        let mut capabilities = vec!["read_files".into()];
        if implement {
            capabilities.push("edit_files".into());
        }
        AdapterProbeReport {
            binary: "opencode-loopback".into(),
            version: "loopback-fixture".into(),
            actual_model: Some(MODEL.into()),
            capabilities,
            roles: vec![if implement { "implement" } else { "review" }.into()],
        }
    }

    pub(crate) fn command_with_payload(
        &self,
        directory: &Path,
        repo: &Path,
        managed: &Path,
        payload: &Value,
        implement: bool,
    ) -> io::Result<Command> {
        let payload = serde_json::to_vec(payload)?;
        if payload.len() > MAX {
            return Err(io::Error::other("OPENCODE_REVIEW_INPUT_LIMIT"));
        }
        let private = directory.canonicalize()?.join("opencode-private");
        fs::DirBuilder::new().mode(0o700).create(&private)?;
        let file = private.join("request");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)?
            .write_all(&payload)?;
        let stdin = File::open(&file)?;
        fs::remove_file(file)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("opencode-review-fixture")
            .arg(&self.executable)
            .arg(self.provider_port.to_string())
            .arg(&private)
            .arg(repo.canonicalize()?)
            .arg(managed.canonicalize()?)
            .arg(if implement { "implement" } else { "review" })
            .env_clear()
            .stdin(Stdio::from(stdin));
        Ok(command)
    }
}

struct ProfileDirectory(PathBuf);
impl ProfileDirectory {
    fn create(parent: &Path) -> Checked<Self> {
        let path = parent.join("profile");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| "OPENCODE_CONFIG")?;
        Ok(Self(path))
    }
}
impl Drop for ProfileDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildScope(Child);
impl Drop for ChildScope {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline && matches!(self.0.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(10));
            }
            // The outer Relay supervisor still owns and checks the entire process group.
        }
    }
}

fn port_of(socket: &TcpListener) -> Checked<u16> {
    Ok(socket.local_addr().map_err(|_| "OPENCODE_PORT")?.port())
}

fn auth_probe(port: u16, auth: Option<&str>) -> Checked<u16> {
    let mut stream = TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(400),
    )
    .map_err(|_| "OPENCODE_CONTROL")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|_| "OPENCODE_CONTROL")?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|_| "OPENCODE_CONTROL")?;
    write!(
        stream,
        "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    )
    .map_err(|_| "OPENCODE_CONTROL")?;
    if let Some(password) = auth {
        write!(
            stream,
            "Authorization: Basic {}\r\n",
            encode_basic(format!("opencode:{password}").as_bytes())
        )
        .map_err(|_| "OPENCODE_CONTROL")?;
    }
    stream.write_all(b"\r\n").map_err(|_| "OPENCODE_CONTROL")?;
    let mut status = String::new();
    BufReader::new(stream)
        .take(128)
        .read_line(&mut status)
        .map_err(|_| "OPENCODE_CONTROL")?;
    status
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("OPENCODE_CONTROL")
}

fn encode_basic(bytes: &[u8]) -> String {
    const MAP: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let (a, b, d) = (
            c[0] as usize,
            c.get(1).copied().unwrap_or(0) as usize,
            c.get(2).copied().unwrap_or(0) as usize,
        );
        out.push(MAP[a >> 2] as char);
        out.push(MAP[((a & 3) << 4) | (b >> 4)] as char);
        out.push(if c.len() > 1 {
            MAP[((b & 15) << 2) | (d >> 6)] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            MAP[d & 63] as char
        } else {
            '='
        });
    }
    out
}

const DENY: &str = r#"(version 1)
(allow default)
(deny file-write* network* process-exec)
(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/var/root")
  (subpath (param "REPO")) (subpath (param "MANAGED")))
(allow process-exec file-read-data (literal (param "CLI")))
(allow file-write* file-read-data (subpath (param "WORK")) (literal "/dev/null"))
"#;

struct Session<'a> {
    exe: &'a Path,
    private: &'a Path,
    repo: &'a Path,
    managed: &'a Path,
    control: u16,
    gateway: u16,
    password: &'a str,
}
impl Session<'_> {
    fn command(&self, server: bool) -> Command {
        let network = if server {
            format!("(allow network-bind network-inbound (local ip \"localhost:{}\"))(allow network-outbound (remote ip \"localhost:{}\"))",self.control,self.gateway)
        } else {
            format!(
                "(allow network-outbound (remote ip \"localhost:{}\"))",
                self.control
            )
        };
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-D")
            .arg(format!("CLI={}", self.exe.display()))
            .arg("-D")
            .arg(format!("WORK={}", self.private.display()))
            .arg("-D")
            .arg(format!("REPO={}", self.repo.display()))
            .arg("-D")
            .arg(format!("MANAGED={}", self.managed.display()))
            .arg("-p")
            .arg(format!("{DENY}{network}"))
            .arg(self.exe);
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.private)
            .env("TMPDIR", self.private.join("tmp"))
            .env("OPENCODE_CONFIG", self.private.join("config.json"))
            .env("OPENCODE_PASSWORD", self.password)
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1")
            .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1")
            .env("OPENCODE_CONFIG_PROJECT_DISABLE", "1")
            .current_dir(self.private)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        c
    }
}

fn request_fixture(port: u16, candidate: &Value) -> Checked<Vec<u8>> {
    let url = format!("http://127.0.0.1:{port}/anthropic/v1/messages");
    let body = json!({"model":MODEL,"stream":true,"max_tokens":1024,
        "messages":[{"role":"user","content":[{"type":"text","text":candidate.to_string()}]}]});
    let mut curl = ChildScope(
        Command::new("/usr/bin/curl")
            .args([
                "--disable",
                "--silent",
                "--fail",
                "--max-time",
                "7",
                "--retry",
                "0",
                "--noproxy",
                "*",
                "--proto",
                "=http",
                "--header",
                "Content-Type: application/json",
                "--data-binary",
                "@-",
                &url,
            ])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "OPENCODE_FIXTURE_TRANSPORT")?,
    );
    curl.0
        .stdin
        .take()
        .ok_or("OPENCODE_FIXTURE_TRANSPORT")?
        .write_all(body.to_string().as_bytes())
        .map_err(|_| "OPENCODE_FIXTURE_TRANSPORT")?;
    let read = crate::service::read_bounded(
        curl.0.stdout.take().ok_or("OPENCODE_FIXTURE_TRANSPORT")?,
        MAX,
        Instant::now() + Duration::from_secs(8),
    );
    if read.oversized || read.timed_out || read.read_error || !bounded_status(&mut curl.0)? {
        return Err("OPENCODE_FIXTURE_TRANSPORT");
    }
    crate::route_pilot::provider_evidence(&read.bytes, MODEL)?;
    Ok(read.bytes)
}

fn review_output(bytes: &[u8]) -> Checked<Value> {
    let text = std::str::from_utf8(bytes).map_err(|_| "OPENCODE_OUTPUT")?;
    let (mut session, mut message, mut result) = (None, None, String::new());
    let mut ended = false;
    for line in text.lines() {
        let item = relay_protocol::frame::decode_json_value(line).map_err(|_| "OPENCODE_OUTPUT")?;
        let sid = item["sessionID"].as_str().ok_or("OPENCODE_OUTPUT")?;
        let part = &item["part"];
        let mid = part["messageID"].as_str().ok_or("OPENCODE_OUTPUT")?;
        if ended || part["sessionID"].as_str() != Some(sid) {
            return Err("OPENCODE_OUTPUT");
        }
        match (item["type"].as_str(), part["type"].as_str()) {
            (Some("step_start"), Some("step-start")) if session.is_none() => {
                session = Some(sid.to_owned());
                message = Some(mid.to_owned());
            }
            (outer, inner)
                if session.as_deref() == Some(sid) && message.as_deref() == Some(mid) =>
            {
                match (outer, inner) {
                    (Some("text"), Some("text")) => {
                        result.push_str(part["text"].as_str().ok_or("OPENCODE_OUTPUT")?)
                    }
                    (Some("step_finish"), Some("step-finish")) if part["reason"] == "stop" => {
                        ended = true
                    }
                    _ => return Err("OPENCODE_OUTPUT"),
                }
            }
            _ => return Err("OPENCODE_OUTPUT"),
        }
        if result.len() > 16 * 1024 {
            return Err("OPENCODE_OUTPUT");
        }
    }
    if !ended {
        return Err("OPENCODE_OUTPUT");
    }
    relay_protocol::frame::decode_json_value(&result).map_err(|_| "OPENCODE_REVIEW_JSON")
}

fn bounded_status(child: &mut Child) -> Checked<bool> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait().map_err(|_| "OPENCODE_EXIT")? {
            Some(status) => return Ok(status.success()),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => return Err("OPENCODE_EXIT"),
        }
    }
}

fn run_session(args: &[String]) -> Checked<Value> {
    let [exe, port, dir, repo, managed, stage] = args else {
        return Err("OPENCODE_CONFIG");
    };
    let provider_port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 1023)
        .ok_or("OPENCODE_CONFIG")?;
    let exe = Path::new(exe);
    if !exe.is_file() || exe.canonicalize().ok().as_deref() != Some(exe) {
        return Err("OPENCODE_CONFIG");
    }
    let mut input = String::new();
    io::stdin()
        .lock()
        .take((MAX + 1) as u64)
        .read_to_string(&mut input)
        .map_err(|_| "OPENCODE_REVIEW_INPUT")?;
    if input.len() > MAX {
        return Err("OPENCODE_REVIEW_INPUT");
    }
    let candidate =
        relay_protocol::frame::decode_json_value(&input).map_err(|_| "OPENCODE_REVIEW_INPUT")?;
    let marker = match stage.as_str() {
        "implement" if candidate["base_oid"].is_string() && candidate["files"].is_array() => {
            EDIT_MARKER
        }
        "review"
            if candidate.as_object().is_some_and(|map| map.len() == 5)
                && candidate["receipt"].is_object()
                && [
                    "goal",
                    "diff",
                    "candidate_diff_sha256",
                    "test_receipt_sha256",
                ]
                .iter()
                .all(|key| candidate[key].is_string()) =>
        {
            MARKER
        }
        _ => return Err("OPENCODE_REVIEW_INPUT"),
    };

    session(
        exe,
        Path::new(dir),
        Path::new(repo),
        Path::new(managed),
        MODEL,
        marker,
        || request_fixture(provider_port, &candidate),
    )
}

pub(crate) fn pilot_session(
    exe: &Path,
    directory: &Path,
    model: &str,
    response: impl FnOnce() -> Checked<Vec<u8>>,
) -> Checked<Value> {
    session(
        exe, directory, directory, directory, model, MARKER, response,
    )
}

pub(crate) fn pilot_review_candidate(
    exe: &Path,
    directory: &Path,
    excluded: &Path,
    model: &str,
    response: impl FnOnce() -> Checked<Vec<u8>>,
) -> Checked<Value> {
    session(exe, directory, excluded, excluded, model, MARKER, response)
}

pub(crate) fn project_session(
    executable: &Path,
    temporary: &Path,
    repo: &Path,
    managed: &Path,
    model: &str,
    response: impl FnOnce() -> Checked<Vec<u8>>,
) -> Checked<Value> {
    session(
        executable, temporary, repo, managed, model, MARKER, response,
    )
}

fn session(
    exe: &Path,
    directory: &Path,
    repo: &Path,
    managed: &Path,
    model: &str,
    marker: &str,
    response: impl FnOnce() -> Checked<Vec<u8>>,
) -> Checked<Value> {
    let profile = ProfileDirectory::create(directory)?;
    let private = profile.0.as_path();
    let gateway = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| "OPENCODE_PORT")?;
    gateway.set_nonblocking(true).map_err(|_| "OPENCODE_PORT")?;
    let gw_port = port_of(&gateway)?;
    let reserve = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| "OPENCODE_PORT")?;
    let control = port_of(&reserve)?;
    let key = crate::route_pilot::nonce()?;
    let password = crate::route_pilot::nonce()?;
    let url = format!("http://127.0.0.1:{gw_port}/anthropic/v1");
    let provider = "minimax-coding-plan";
    let denials = [
        "tool.use",
        "mcp.use",
        "skill.use",
        "plugin.use",
        "websearch.use",
    ]
    .map(|a| json!({"action":a,"resource":"*","effect":"deny"}));
    let config = json!({
        "enabled_providers":[provider],"autoupdate":false,"share":"disabled",
        "instructions":[],"plugins":[],"mcp":{},"policies":denials,
        "permissions":[{"action":"*","resource":"*","effect":"deny"}],
        "agents":{"relay-review":{
            "description":"Read-only Relay review","mode":"primary",
            "permissions":[{"action":"*","resource":"*","effect":"deny"}],
        }},
        "provider":{(provider):{
            "npm":"@ai-sdk/anthropic","name":"Relay controlled gateway","options":{"baseURL":url,"apiKey":key},
            "models":{(model):{"name":model,"limit":{"context":32768,"output":1024}}},
        }},
    });
    fs::DirBuilder::new()
        .mode(0o700)
        .create(private.join("tmp"))
        .map_err(|_| "OPENCODE_CONFIG")?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(private.join("config.json"))
        .and_then(|mut file| file.write_all(config.to_string().as_bytes()))
        .map_err(|_| "OPENCODE_CONFIG")?;
    let isolation = Session {
        exe,
        private,
        repo,
        managed,
        control,
        gateway: gw_port,
        password: &password,
    };

    drop(reserve);
    let mut server = ChildScope(
        isolation
            .command(true)
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &control.to_string(),
            ])
            .spawn()
            .map_err(|_| "OPENCODE_SERVER_START")?,
    );
    let hello = server.0.stdout.take().ok_or("OPENCODE_SERVER_START")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = tx.send(
            BufReader::new(hello)
                .take(512)
                .read_line(&mut line)
                .map(|_| line),
        );
    });
    let banner = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "OPENCODE_SERVER_START")?
        .map_err(|_| "OPENCODE_SERVER_START")?;
    if banner.trim_end() != format!("server listening on http://127.0.0.1:{control}")
        || !matches!(server.0.try_wait(), Ok(None))
    {
        return Err("OPENCODE_SERVER_START");
    }
    if auth_probe(control, None)? != 401
        || auth_probe(control, Some("invalid"))? != 401
        || auth_probe(control, Some(&password))? != 200
        || !matches!(server.0.try_wait(), Ok(None))
    {
        return Err("OPENCODE_CONTROL_AUTH");
    }

    let mut client = ChildScope(
        isolation
            .command(false)
            .args([
                "--server",
                &format!("http://127.0.0.1:{control}"),
                "run",
                "--archived",
                "--title",
                "Relay controlled review",
                "--agent",
                "relay-review",
                "--format",
                "json",
                "--model",
                &format!("{provider}/{model}"),
                marker,
            ])
            .spawn()
            .map_err(|_| "OPENCODE_CLIENT_START")?,
    );
    let stdout = client.0.stdout.take().ok_or("OPENCODE_CLIENT_START")?;
    let result = std::thread::spawn(move || {
        crate::service::read_bounded(stdout, MAX, Instant::now() + Duration::from_secs(38))
    });
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut connection = loop {
        match gateway.accept() {
            Ok((connection, _)) => break connection,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    && Instant::now() < deadline
                    && matches!(client.0.try_wait(), Ok(None)) =>
            {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => return Err("OPENCODE_GATEWAY_NO_REQUEST"),
        }
    };
    drop(gateway);
    crate::route_pilot::admit_route(&mut connection, &key, model, Some(marker))?;
    let response = response()?;
    write!(connection,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len())
        .and_then(|()| connection.write_all(&response)).map_err(|_| "OPENCODE_GATEWAY_RESPONSE")?;
    drop(connection);
    let out = result.join().map_err(|_| "OPENCODE_OUTPUT")?;
    if out.timed_out || out.oversized || out.read_error || !bounded_status(&mut client.0)? {
        return Err("OPENCODE_OUTPUT");
    }
    review_output(&out.bytes)
}

pub fn run(args: &[String]) -> Result<(), u8> {
    println!("{}", json!({"type":"worker_started"}));
    match run_session(args) {
        Ok(report) => {
            println!("{}", json!({"type":"review_verdict","report":report}));
            println!(
                "{}",
                json!({"type":"worker_finished","termination":"success"})
            );
            Ok(())
        }
        Err(code) => {
            println!(
                "{}",
                json!({"type":"worker_finished","termination":"unsupported_protocol","diagnostic":code})
            );
            Err(1)
        }
    }
}
