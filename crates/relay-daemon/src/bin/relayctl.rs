//! relayctl：M1 命令行入口（04）。
//!
//! - 业务命令：经 Unix socket 与 daemon 共用业务契约（每连接 hello +
//!   至多一条业务请求）。
//! - `daemon`：前台运行 daemon（M1 业务模式，读临时配置）。
//! - `e2e`：可重跑端到端入口——临时 HOME/仓库/实例 + 固定样本跑通
//!   plan → submit → 领取 → 栅栏 → mock → 收据 → 发布登记 → 验收。
//! - `mcp`：stdio MCP 服务器（只宣告 Tools；stdout 只输出协议）。

use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

use relay_daemon::claude_wire;
#[path = "local_preview/mod.rs"]
mod local_preview;
#[path = "maintenance/mod.rs"]
mod maintenance;
#[path = "project_setup/mod.rs"]
mod project_setup;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::from(0),
        Err(code) => ExitCode::from(code),
    }
}

fn run(args: &[String]) -> Result<(), u8> {
    // 允许 "--socket PATH <cmd>" 与 "<cmd> ... --socket PATH" 两种写法：
    // 逐项扫描，跳过旗标与其取值（--socket 带一个值），第一个其余
    // 参数即命令。
    let mut cmd: Option<&str> = None;
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--socket"
            || arg == "--instance-dir"
            || arg == "--config"
            || arg == "--work-dir"
            || arg == "--catalog"
            || arg == "--scenario"
        {
            index += 2;
            continue;
        }
        if arg.starts_with("--") {
            index += 1;
            continue;
        }
        cmd = Some(arg);
        break;
    }
    let Some(cmd) = cmd else {
        eprintln!("用法: relayctl [--socket PATH] <plan|submit|status|list|artifact|cancel|retry|review|events|daemon|e2e|demo-auto|doctor|maintenance|migrate|gate|project|mcp|mcp-verify|adapter-check|route-pilot|catalog|version> [参数]");
        return Err(2);
    };
    match cmd {
        "e2e" => e2e::run(&args[1..]),
        "demo-auto" => demo_auto::run(&args[1..]),
        "local-preview" => local_preview::run(&args[1..]),
        "project-setup" => project_setup::run(&args[1..]),
        "project-route-worker" => relay_daemon::live_routes::worker(&args[1..]),
        "maintenance" => maintenance::run(&args[1..]),
        "catalog" => relay_daemon::model_catalog::cli(&args[1..]),
        "version" => {
            // 09：产品 semver、业务版本、wire 与 DB schema 各自记录，互不混淆。
            println!(
                "{}",
                json!({
                    "product": env!("CARGO_PKG_VERSION"),
                    "business": "relay.v1",
                    "wire": 1,
                    "db_schema": relay_store::SCHEMA_VERSION,
                })
            );
            Ok(())
        }
        "mcp-verify" => mcp_verify::run(&args[1..]),
        "adapter-check" => adapter_check::run(&args[1..]),
        "opencode-review-fixture" => relay_daemon::opencode_review::run(&args[1..]),
        "route-pilot" => relay_daemon::route_pilot::run(&args[1..]),
        "adapter-verify-claude" => claude_wire::run(&args[1..]),
        "doctor" => doctor::run(&args[1..]),
        "gate" => gate_cmd::run(&args[1..]),
        "migrate" => {
            let Some(instance) = flag(args, "--instance-dir").map(PathBuf::from) else {
                eprintln!("用法: relayctl migrate --instance-dir <dir>");
                return Err(2);
            };
            match relay_daemon::migrate_instance_v8_to_v9(&instance) {
                Ok(backup) => {
                    println!("数据库已迁移至 v9。v8 备份：{}", backup.display());
                    Ok(())
                }
                Err(err) => {
                    eprintln!("迁移失败：{err}");
                    Err(1)
                }
            }
        }
        "daemon" => daemon_cmd::run(&args[1..]),
        "mcp" => mcp::run(&args[1..]),
        _ => {
            let Some(socket) = flag(args, "--socket") else {
                eprintln!("缺少 --socket PATH");
                return Err(2);
            };
            let method = match cmd {
                "plan" => "relay_plan",
                "submit" => "relay_submit",
                "status" => "relay_status",
                "list" => "relay_list",
                "artifact" => "relay_artifact",
                "cancel" => "relay_cancel",
                "retry" => "relay_retry",
                "review" => "relay_review",
                "events" => "events.list",
                "project" => "ui.project",
                other => {
                    eprintln!("未知命令 {other}");
                    return Err(2);
                }
            };
            let params = if cmd == "project" {
                json!({})
            } else {
                build_params(cmd, args)?
            };
            match client::call(std::path::Path::new(&socket), method, params) {
                Ok(value) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                    );
                    if value.get("error").is_some() {
                        Err(1)
                    } else {
                        Ok(())
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    Err(1)
                }
            }
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn positional_after(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut hit = false;
    for a in args {
        if hit {
            if a.starts_with("--") {
                hit = false;
            } else {
                out.push(a.clone());
                continue;
            }
        }
        if a == name {
            hit = true;
        }
    }
    out
}

/// 数值旗标解析（统一错误处理）。
fn num_flag(args: &[String], name: &str) -> Result<u64, u8> {
    let raw = flag(args, name).ok_or_else(|| {
        eprintln!("缺少 {name}");
        2u8
    })?;
    raw.parse::<u64>().map_err(|_| {
        eprintln!("{name} 不是整数");
        2u8
    })
}

/// 业务参数构造（M1 最小旗标集；字段约束由 daemon 端 DTO 校验）。
fn build_params(cmd: &str, args: &[String]) -> Result<Value, u8> {
    let need = |name: &str| -> Result<String, u8> {
        flag(args, name).ok_or_else(|| {
            eprintln!("缺少 {name}");
            2u8
        })
    };
    let value = match cmd {
        "plan" => {
            let scope: Vec<String> = positional_after(args, "--scope");
            let rules: Vec<String> = positional_after(args, "--rules");
            let mut params = json!({
                "project_id": need("--project-id")?,
                "goal": need("--goal")?,
                "brief": need("--brief")?,
                "write_scope": scope,
                "profile_id": need("--profile-id")?,
                "verification_rule_ids": rules,
            });
            if let Some(oid) = flag(args, "--base-oid") {
                params["base_oid"] = json!(oid);
            }
            params
        }
        "submit" => json!({
            "plan_id": need("--plan-id")?,
            "request_id": need("--request-id")?,
        }),
        "status" => json!({
            "task_id": need("--task-id")?,
        }),
        "list" => {
            let mut params = json!({"project_id": need("--project-id")?});
            if let Some(c) = flag(args, "--cursor") {
                params["cursor"] = json!(c);
            }
            if flag(args, "--limit").is_some() {
                params["limit"] = json!(num_flag(args, "--limit")?);
            }
            params
        }
        "artifact" => {
            let mut params = json!({
                "task_id": need("--task-id")?,
                "attempt_no": num_flag(args, "--attempt-no")?,
                "kind": need("--kind")?,
            });
            if flag(args, "--offset").is_some() {
                params["offset"] = json!(num_flag(args, "--offset")?);
            }
            if flag(args, "--limit").is_some() {
                params["limit"] = json!(num_flag(args, "--limit")?);
            }
            params
        }
        "cancel" => json!({
            "task_id": need("--task-id")?,
            "expected_revision": num_flag(args, "--expected-revision")?,
            "request_id": need("--request-id")?,
        }),
        "retry" => json!({
            "task_id": need("--task-id")?,
            "plan_id": need("--plan-id")?,
            "expected_revision": num_flag(args, "--expected-revision")?,
            "request_id": need("--request-id")?,
        }),
        "review" => {
            let mut params = json!({
                "task_id": need("--task-id")?,
                "attempt_no": num_flag(args, "--attempt-no")?,
                "expected_revision": num_flag(args, "--expected-revision")?,
                "request_id": need("--request-id")?,
                "verdict": need("--verdict")?,
                "diff_sha256": need("--diff-sha256")?,
                "test_sha256": need("--test-sha256")?,
            });
            if let Some(note) = flag(args, "--note") {
                params["note"] = json!(note);
            }
            params
        }
        "events" => {
            let mut params = json!({});
            if flag(args, "--after-seq").is_some() {
                params["after_seq"] = json!(num_flag(args, "--after-seq")?);
            }
            if flag(args, "--limit").is_some() {
                params["limit"] = json!(num_flag(args, "--limit")?);
            }
            params
        }
        other => {
            eprintln!("未知业务命令 {other}");
            return Err(2);
        }
    };
    Ok(value)
}

/// 演示/验收入口的工作目录（--work-dir 指向已有路径时不在其上操作，
/// 而是在其下新建本次独占子目录——已有目录与内容不受影响；不存在的
/// 路径创建为本次根；未给出时在临时目录下按 `default_prefix` 命名）。
/// 子目录前缀须短：实例内 socket 路径受 macOS sun_path 上限约束。
fn resolve_run_dir(
    args: &[String],
    default_prefix: &str,
    subdir_prefix: &str,
) -> Result<PathBuf, u8> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        % 1_000_000_000_000;
    match flag(args, "--work-dir").map(PathBuf::from) {
        Some(existing) if existing.exists() => {
            let run_dir = existing.join(format!("{subdir_prefix}{nanos}"));
            std::fs::create_dir_all(&run_dir).map_err(|e| {
                eprintln!("在已有目录下创建独占子目录失败：{e}");
                1u8
            })?;
            Ok(run_dir)
        }
        Some(path) => {
            std::fs::create_dir_all(&path).map_err(|e| {
                eprintln!("创建工作目录失败：{e}");
                1u8
            })?;
            Ok(path)
        }
        None => Ok(std::env::temp_dir().join(format!("{default_prefix}{nanos}"))),
    }
}

/// socket 客户端：每连接 hello + 至多一条业务请求（04）。
pub mod client {
    use super::*;
    use std::io::Read;

    pub fn send_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        stream.write_all(&bytes).map_err(|e| e.to_string())
    }

    pub fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>, String> {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => return Ok(if buf.is_empty() { None } else { Some(buf) }),
                Ok(_) => {
                    buf.push(byte[0]);
                    if byte[0] == b'\n' {
                        return Ok(Some(buf));
                    }
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    /// hello 握手 + 一条业务请求；返回完整响应帧（成功 result 或业务 error）。
    /// MCP 桥需要保留结构化业务错误；普通 CLI 用 [`call`] 取 result。
    pub fn call_full(
        socket: &std::path::Path,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut stream = UnixStream::connect(socket).map_err(|e| format!("连接失败：{e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        send_frame(
            &mut stream,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"version": 1}}),
        )?;
        let confirm = read_frame(&mut stream)?.ok_or("握手确认缺失")?;
        let confirm: Value = serde_json::from_slice(&confirm).map_err(|e| e.to_string())?;
        if confirm["result"]["version"] != json!(1) {
            return Err(format!("握手确认异常: {confirm}"));
        }
        send_frame(
            &mut stream,
            &json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}),
        )?;
        let reply = read_frame(&mut stream)?.ok_or("业务响应缺失（连接被关闭）")?;
        serde_json::from_slice(&reply).map_err(|e| e.to_string())
    }

    /// hello 握手 + 一条业务请求；返回业务结果或错误文本。
    pub fn call(socket: &std::path::Path, method: &str, params: Value) -> Result<Value, String> {
        let reply = call_full(socket, method, params)?;
        if let Some(error) = reply.get("error") {
            let code = error["code"].as_str().unwrap_or("UNKNOWN");
            let message = error["message"].as_str().unwrap_or("业务错误");
            return Err(format!("业务错误 {code}: {message}"));
        }
        Ok(reply["result"].clone())
    }
}

/// relayctl daemon：前台运行 M1 daemon（临时配置）。
pub mod daemon_cmd {
    use super::*;

    pub fn run(args: &[String]) -> Result<(), u8> {
        let Some(instance) = flag(args, "--instance-dir").map(PathBuf::from) else {
            eprintln!("缺少 --instance-dir");
            return Err(2);
        };
        // 无 --config：骨架模式（只读 status 替身），供 MCP 发现验收。
        let config_text = match flag(args, "--config") {
            Some(path) => Some(std::fs::read_to_string(&path).map_err(|e| {
                eprintln!("读取配置失败：{e}");
                1u8
            })?),
            None => None,
        };
        relay_sys::install_stop_signals().map_err(|e| {
            eprintln!("注册停止信号失败：{e}");
            1u8
        })?;
        let daemon = match &config_text {
            Some(text) => {
                let config: Value = serde_json::from_str(text).map_err(|e| {
                    eprintln!("配置不是合法 JSON：{e}");
                    1u8
                })?;
                let service_config = relay_daemon::service::ServiceConfig::from_json(&config)
                    .map_err(|e| {
                        eprintln!("配置无效：{e}");
                        1u8
                    })?;
                relay_daemon::start_m1(
                    &instance,
                    &crate::now_text(),
                    relay_daemon::ipc::IpcConfig::default(),
                    service_config,
                )
                .map_err(|e| {
                    eprintln!("daemon 启动失败：{e}");
                    1u8
                })?
            }
            None => relay_daemon::start(&instance, &crate::now_text()).map_err(|e| {
                eprintln!("daemon 启动失败：{e}");
                1u8
            })?,
        };
        eprintln!(
            "relay daemon ready: socket={}",
            daemon.socket_path().display()
        );
        let mut daemon = daemon;
        daemon.serve_until(relay_sys::stop_requested);
        Ok(())
    }
}

/// relayctl e2e：可重跑端到端入口（固定样本，正例整链 + 关键核对）。
pub mod e2e {
    use super::*;
    use relay_git::sha256_hex;
    use std::process::Command;

    pub fn run(args: &[String]) -> Result<(), u8> {
        let work_dir = resolve_run_dir(args, "m1e", "e")?;
        let keep = args.iter().any(|a| a == "--keep");
        eprintln!("M1 e2e 工作目录：{}", work_dir.display());

        let outcome = run_chain(&work_dir);
        if outcome.is_err() && !keep {
            let _ = std::fs::remove_dir_all(&work_dir);
        }
        match outcome {
            Ok(summary) => {
                eprintln!("{summary}");
                eprintln!("M1 e2e 通过（工作目录保留：{}）", work_dir.display());
                Ok(())
            }
            Err(step_err) => {
                eprintln!("M1 e2e 失败：{step_err}");
                Err(1)
            }
        }
    }

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "relay-e2e")
            .env("GIT_AUTHOR_EMAIL", "relay@example.invalid")
            .env("HOME", root.parent().unwrap_or(root))
            .output()
            .expect("git 启动失败");
        assert!(
            output.status.success(),
            "git {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn run_chain(work_dir: &std::path::Path) -> Result<String, String> {
        // 1. 临时 HOME（隔离真实用户配置）与临时仓库。
        let home = work_dir.join("home");
        let repo = work_dir.join("repo");
        let instance = work_dir.join("instance");
        let managed = work_dir.join("managed");
        for path in [&home, &repo, &instance, &managed] {
            std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        }
        // 实例目录严格 0700（02/A2a）。
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&instance, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let repo_git = |args: &[&str]| git(&repo, args);
        repo_git(&["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(repo.join("src")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(repo.join("tests")).map_err(|e| e.to_string())?;
        std::fs::write(
            repo.join("src/count.rs"),
            "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(repo.join("tests/count.rs"), "// placeholder\n")
            .map_err(|e| e.to_string())?;
        repo_git(&["add", "src/count.rs", "tests/count.rs"]);
        repo_git(&["commit", "-q", "-m", "baseline"]);
        let head_before = repo_git(&["rev-parse", "HEAD"]);
        let head_after = head_before.clone();
        let index_before = repo_git(&["status", "--porcelain"]);

        // 2. 临时配置（mock worker 与规则脚本来自项目自带 mock/）。
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mock_worker = manifest.join("mock/mock_worker.sh");
        let rule_pass = manifest.join("mock/mock_rule_pass.sh");
        let config = json!({
            "repo_root": repo.display().to_string(),
            "managed_root": managed.display().to_string(),
            "mock_worker": mock_worker.display().to_string(),
            "worker_timeout_ms": 30_000,
            "rules": [
                {"id": "mock_rule", "executable": rule_pass.display().to_string(),
                 "argv": [], "timeout_ms": 5_000}
            ],
        });
        let config_path = work_dir.join("relay-config.json");
        std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
            .map_err(|e| e.to_string())?;

        // 3. 启动 M1 daemon（真实 Store + 真实 Unix socket）。
        let mut daemon = relay_daemon::start_m1(
            &instance,
            &crate::now_text(),
            relay_daemon::ipc::IpcConfig::default(),
            relay_daemon::service::ServiceConfig::from_json(&config).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("daemon 启动失败：{e}"))?;
        daemon.serve_background();
        let socket = daemon.socket_path().to_owned();
        eprintln!("daemon ready：{}", socket.display());

        // 4. 业务链：plan → submit（幂等重送）→ 等待 awaiting_review。
        let plan = client::call(
            &socket,
            "relay_plan",
            json!({
                "project_id": "proj_e2e",
                "goal": "修复块计数在空输入时的返回值",
                "brief": "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"],
                "base_oid": head_after,
            }),
        )?;
        let plan_id = plan["plan_id"]
            .as_str()
            .ok_or("plan 缺少 plan_id")?
            .to_owned();
        eprintln!("plan: {plan_id}");

        let request_id = "3fa85f64-5717-4562-b3fc-2c963f66afa6";
        let task = client::call(
            &socket,
            "relay_submit",
            json!({
                "plan_id": plan_id, "request_id": request_id,
            }),
        )?;
        let task_id = task["task"]["id"]
            .as_str()
            .ok_or("submit 缺少 task id")?
            .to_owned();
        let revision0 = task["task"]["revision"]
            .as_u64()
            .ok_or("submit 缺少 revision")?;
        // 幂等重送：同 request 同参 → 同一 task（T04）。
        let task_again = client::call(
            &socket,
            "relay_submit",
            json!({
                "plan_id": plan_id, "request_id": request_id,
            }),
        )?;
        let task_id_again = task_again["task"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if task_id_again != task_id {
            return Err(format!(
                "幂等重送得到不同 task：{task_id} vs {task_id_again}"
            ));
        }
        eprintln!("task: {task_id}（幂等重送一致）");

        // 5. 等待 awaiting_review（worker：领取→栅栏→mock→收据→发布登记）。
        let mut attempt_state = String::new();
        for _ in 0..200 {
            let status = client::call(&socket, "relay_status", json!({"task_id": task_id}))?;
            attempt_state = status["task"]["state"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if attempt_state == "awaiting_review" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if attempt_state != "awaiting_review" {
            return Err(format!("任务未进入 awaiting_review：{attempt_state}"));
        }
        let status = client::call(&socket, "relay_status", json!({"task_id": task_id}))?;
        let revision = status["task"]["revision"]
            .as_u64()
            .ok_or("status 缺少 revision")?;
        let attempt_no = status["task"]["active_attempt"]
            .as_u64()
            .ok_or("status 缺少 attempt")?;

        // 6. 按摘要验收：读取登记制品并独立核对哈希（attempt/revision/补丁/收据）。
        let manifest = client::call(
            &socket,
            "relay_artifact",
            json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "manifest",
            }),
        )?;
        let diff = client::call(
            &socket,
            "relay_artifact",
            json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "diff",
            }),
        )?;
        let receipt = client::call(
            &socket,
            "relay_artifact",
            json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "test_receipt",
            }),
        )?;
        let manifest_json = manifest["content"]
            .as_str()
            .ok_or("manifest 缺少 content")?;
        let patch_text = diff["content"].as_str().ok_or("diff 缺少 content")?;
        let receipt_text = receipt["content"].as_str().ok_or("receipt 缺少 content")?;
        let patch_sha = sha256_hex(patch_text.as_bytes());
        let receipt_sha = sha256_hex(receipt_text.as_bytes());
        // manifest 绑定同一 attempt、基线与补丁摘要。
        if !manifest_json.contains(&format!("\"attempt\":\"{task_id}-{attempt_no}\"")) {
            return Err("manifest 未绑定本 attempt".to_owned());
        }
        if !manifest_json.contains(&format!("\"base_oid\":\"{head_after}\"")) {
            return Err("manifest 未绑定基线".to_owned());
        }
        let declared_patch_sha = manifest_json
            .split("\"patch_sha256\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .ok_or("manifest 缺少 patch_sha256")?;
        if declared_patch_sha != patch_sha {
            return Err(format!(
                "manifest 声明摘要与实际补丁不符：{declared_patch_sha} vs {patch_sha}"
            ));
        }
        eprintln!("摘要核对：patch={patch_sha}");

        // review accept → completed。
        let reviewed = client::call(
            &socket,
            "relay_review",
            json!({
                "task_id": task_id, "attempt_no": attempt_no,
                "expected_revision": revision, "request_id": "8f14e45f-ceea-467f-a8aa-88fb4d4bd16b",
                "verdict": "accept", "diff_sha256": patch_sha, "test_sha256": receipt_sha,
            }),
        )?;
        if reviewed["task"]["state"] != json!("completed") {
            return Err(format!("验收后未完成：{reviewed}"));
        }
        let _ = revision0;

        // 7. 主 checkout 不变（T05）：HEAD、index、用户文件原样。
        let head_now = repo_git(&["rev-parse", "HEAD"]);
        if head_now != head_before {
            return Err(format!(
                "主 checkout HEAD 被改动：{head_before} → {head_now}"
            ));
        }
        if repo_git(&["status", "--porcelain"]) != index_before {
            return Err("主 checkout index/文件被改动".to_owned());
        }
        // 补丁能在干净 checkout 应用（T05）。
        let clean = work_dir.join("clean-checkout");
        std::fs::create_dir_all(&clean).map_err(|e| e.to_string())?;
        git(&clean, &["clone", "-q", &repo.display().to_string(), "."]);
        let patch_path = work_dir.join("verify.patch");
        std::fs::write(&patch_path, patch_text).map_err(|e| e.to_string())?;
        let apply = Command::new("git")
            .arg("-C")
            .arg(&clean)
            .args([
                "apply",
                "--check",
                patch_path.display().to_string().as_str(),
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !apply.status.success() {
            return Err(format!(
                "补丁未能在干净 checkout 应用：{}",
                String::from_utf8_lossy(&apply.stderr)
            ));
        }
        // 事件查询（events.list）含整链事件。
        let events = client::call(
            &socket,
            "events.list",
            json!({"after_seq": 0, "limit": 100}),
        )?;
        let kinds: Vec<String> = events["events"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["kind"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        for expected in [
            "submitted",
            "claim",
            "prepare_launch",
            "spawned",
            "worker_succeeded",
            "verification_passed",
        ] {
            if !kinds.iter().any(|k| k.contains(expected)) {
                return Err(format!("事件链缺少 {expected}"));
            }
        }

        drop(daemon);
        Ok(format!(
            "验收通过：task={task_id} attempt={attempt_no} revision={revision} patch_sha={patch_sha}"
        ))
    }
}

/// relayctl mcp：stdio MCP 服务器（04：MCP 以 relayctl mcp 的 stdio 运行；
/// 只宣告 Tools；stdout 只输出协议）。
pub mod mcp {
    use super::*;
    use crate::client;

    const PROTOCOL_VERSION: &str = "2025-06-18";

    pub fn run(args: &[String]) -> Result<(), u8> {
        let Some(socket) = flag(args, "--socket") else {
            eprintln!("缺少 --socket PATH");
            return Err(2);
        };
        let socket = PathBuf::from(socket);
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                // 协议层畸形：JSON-RPC 解析错误。
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({
                        "jsonrpc": "2.0", "id": Value::Null,
                        "error": {"code": -32700, "message": "Parse error"},
                    })
                );
                continue;
            };
            let reply = handle(&socket, &msg);
            if let Some(reply) = reply {
                writeln!(writer, "{reply}").map_err(|_| 1u8)?;
            }
        }
        Ok(())
    }

    /// 已公布的工具（04 表顺序；清单顺序固定）。桥只转发这张名单内的
    /// 方法——不向用户转发原始 RPC 管理接口（events.list、ui.overview 等）。
    const TOOLS: [(&str, &str); 8] = [
        ("relay_plan", "预览并登记执行计划（无外发）"),
        ("relay_submit", "提交任务（幂等）"),
        ("relay_status", "查询任务状态"),
        ("relay_list", "列出项目任务"),
        ("relay_artifact", "读取登记制品的有界切片"),
        ("relay_cancel", "取消任务"),
        ("relay_retry", "重试任务"),
        ("relay_review", "人工验收（accept/reject）"),
    ];

    fn schema(tool: &str) -> Value {
        let id = || json!({"type":"string","minLength":1});
        let request = || json!({"type":"string","format":"uuid"});
        let revision = || json!({"type":"integer","minimum":1});
        let attempt = || json!({"type":"integer","minimum":1,"maximum":u32::MAX});
        let digest = || json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
        let text = |max: u32, description: &str| {
            json!({
                "type":"string","maxLength":max,"description":description
            })
        };
        let (required, properties): (&[&str], Value) = match tool {
            "relay_plan" => (
                &[
                    "project_id",
                    "goal",
                    "brief",
                    "profile_id",
                    "write_scope",
                    "verification_rule_ids",
                ],
                json!({
                    "project_id":id(),"profile_id":id(),
                    "goal":text(1024,"目标，UTF-8 最多 1024 字节"),
                    "brief":text(16384,"工作说明，UTF-8 最多 16384 字节"),
                    "write_scope":{"type":"array","maxItems":64,"items":text(240,"仓库相对路径，UTF-8 最多 240 字节")},
                    "verification_rule_ids":{"type":"array","minItems":1,"maxItems":8,"uniqueItems":true,"items":id()},
                    "base_oid":{"type":"string","pattern":"^(?:[0-9a-f]{40}|[0-9a-f]{64})$"}
                }),
            ),
            "relay_submit" => (
                &["plan_id", "request_id"],
                json!({"plan_id":id(),"request_id":request()}),
            ),
            "relay_status" => (&["task_id"], json!({"task_id":id()})),
            "relay_list" => (
                &["project_id"],
                json!({
                    "project_id":id(),"cursor":id(),"limit":{"type":"integer","minimum":1,"maximum":50,"default":20}
                }),
            ),
            "relay_artifact" => (
                &["task_id", "attempt_no", "kind"],
                json!({
                    "task_id":id(),"attempt_no":attempt(),"kind":{"type":"string","enum":relay_protocol::dto::ArtifactRequest::KINDS},
                    "offset":{"type":"integer","minimum":0,"default":0},
                    "limit":{"type":"integer","minimum":1,"maximum":32768,"default":32768}
                }),
            ),
            "relay_cancel" => (
                &["task_id", "expected_revision", "request_id"],
                json!({
                    "task_id":id(),"expected_revision":revision(),"request_id":request()
                }),
            ),
            "relay_retry" => (
                &["task_id", "plan_id", "expected_revision", "request_id"],
                json!({
                    "task_id":id(),"plan_id":id(),"expected_revision":revision(),"request_id":request()
                }),
            ),
            "relay_review" => (
                &[
                    "task_id",
                    "attempt_no",
                    "expected_revision",
                    "request_id",
                    "verdict",
                    "diff_sha256",
                    "test_sha256",
                ],
                json!({
                    "task_id":id(),"attempt_no":attempt(),"expected_revision":revision(),"request_id":request(),
                    "verdict":{"type":"string","enum":["accept","reject"]},"diff_sha256":digest(),"test_sha256":digest(),
                    "note":text(2048,"验收备注，UTF-8 最多 2048 字节")
                }),
            ),
            _ => unreachable!("unadvertised tool"),
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }

    fn tools() -> Value {
        json!(TOOLS
            .iter()
            .map(|(name, description)| {
                json!({"name": name, "description": description, "inputSchema": schema(name)})
            })
            .collect::<Vec<_>>())
    }

    fn handle(socket: &std::path::Path, msg: &Value) -> Option<Value> {
        let id = msg["id"].clone();
        let method = msg["method"].as_str()?.to_owned();
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match method.as_str() {
            "initialize" => Some(reply(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "relayctl-mcp", "version": env!("CARGO_PKG_VERSION")},
            }))),
            "notifications/initialized" => None,
            "tools/list" => Some(reply(json!({"tools": tools()}))),
            "tools/call" => {
                let name = msg["params"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                // 名单外的名字不转发：JSON-RPC 参数错误（线协议层），不是
                // MCP 业务结果。
                if !TOOLS.iter().any(|(tool, _)| *tool == name) {
                    return Some(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "unknown tool"},
                    }));
                }
                let args = msg["params"]["arguments"].clone();
                // 转发到 daemon（同一业务契约）。业务失败必须保留
                // structuredContent 的稳定 code/message/action，供 Host
                // 分支；传输/协议失败也给出同一结构的 SERVICE_UNAVAILABLE。
                let outcome = client::call_full(socket, &name, args);
                Some(match outcome {
                    Ok(full) => {
                        let structured = if full.get("error").is_some() {
                            full["error"].clone()
                        } else {
                            full["result"].clone()
                        };
                        let is_error = full.get("error").is_some()
                            || structured.get("error").is_some_and(Value::is_object);
                        let text = serde_json::to_string_pretty(&structured)
                            .unwrap_or_else(|_| "{}".to_owned());
                        reply(json!({
                            "content": [{"type": "text", "text": text}],
                            "structuredContent": structured,
                            "isError": is_error,
                        }))
                    }
                    Err(text) => {
                        let structured = json!({
                            "schema_version": 1,
                            "error": {
                                "code": "SERVICE_UNAVAILABLE",
                                "message": text,
                                "action": "check_service",
                            },
                        });
                        reply(json!({
                            "content": [{"type": "text", "text":
                                serde_json::to_string_pretty(&structured)
                                    .unwrap_or_else(|_| "{}".to_owned())}],
                            "structuredContent": structured,
                            "isError": true,
                        }))
                    }
                })
            }
            other => Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("方法不存在: {other}")},
            })),
        }
    }
}

/// 共享时间文本（daemon 命令用）。
pub fn now_text() -> String {
    relay_daemon::service::now_rfc3339()
}

/// relayctl doctor：M2 运维检查入口（离线，只读；09）。
///
/// 检查项：实例目录权限 0700 与属主、SQLite schema 版本与 artifacts 表、
/// IPC socket 状态（活跃/陈旧残留/外来）、daemon.lock 可锁性、归属记录。
/// 全部通过退出码 0；任一失败退出码 1 并给出处置提示。
pub mod doctor {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    pub fn run(args: &[String]) -> Result<(), u8> {
        let Some(instance) = flag(args, "--instance-dir").map(PathBuf::from) else {
            eprintln!("缺少 --instance-dir");
            return Err(2);
        };
        let failures = std::cell::Cell::new(0usize);
        let report = |ok: bool, name: &str, hint: &str| {
            if ok {
                println!("ok    {name}");
            } else {
                failures.set(failures.get() + 1);
                println!("FAIL  {name} —— {hint}");
            }
        };

        // 1. 实例目录：存在、真实目录、0700、本 UID 属主。
        let meta = std::fs::symlink_metadata(&instance);
        report(
            meta.as_ref().is_ok_and(|m| m.is_dir() && !m.is_symlink()),
            "实例目录存在且为真实目录",
            "创建实例目录；符号链接一律拒绝",
        );
        if let Ok(m) = &meta {
            use std::os::unix::fs::MetadataExt;
            report(
                m.permissions().mode() & 0o7777 == 0o700,
                "实例目录权限严格 0700",
                "chmod 700 <instance-dir>",
            );
            let own = rustix::process::geteuid().as_raw();
            report(
                m.uid() == own,
                &format!("实例目录属主为当前用户 uid {own}"),
                "以属主身份运行，或 chown 后重试",
            );
        }

        // 2. SQLite：可读、schema 版本、artifacts 表。
        let db_path = instance.join("relay.sqlite3");
        if db_path.exists() {
            match relay_store::Store::open_read_only(&db_path) {
                Ok(store) => {
                    let conn = store.conn().unwrap();
                    let version: String = conn
                        .query_row(
                            "SELECT value FROM meta WHERE key = 'schema_version'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap_or_default();
                    report(
                        version == relay_store::SCHEMA_VERSION.to_string(),
                        &format!(
                            "schema 版本 = {version}（本构建支持 {}）",
                            relay_store::SCHEMA_VERSION
                        ),
                        "v8 实例请停机后运行 relayctl migrate；更旧版本用 relay-store 逐版迁移",
                    );
                    let artifacts: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='artifacts'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    report(
                        artifacts == 1,
                        "artifacts 表存在（v4 制品登记）",
                        "迁移到 v4",
                    );
                }
                Err(e) => report(
                    false,
                    "SQLite 可打开",
                    &format!("打开失败：{e}——schema 未知更高版本或文件损坏，按 09 处置"),
                ),
            }
        } else {
            println!("skip  SQLite（尚无数据库：实例未启动过）");
        }

        // 2.5 派发门：未确认退出的受管进程组（实例级阻断）。
        if db_path.exists() {
            if let Ok(store) = relay_store::Store::open_read_only(&db_path) {
                let gates = store.list_pending_gates().unwrap_or_default();
                let blocking = store
                    .list_managed_groups(Some("blocking"))
                    .unwrap_or_default();
                if gates.is_empty() && blocking.is_empty() {
                    println!("ok    派发门：无阻断（instance-level dispatch gate clear）");
                } else {
                    failures.set(failures.get() + 1);
                    for gate in &gates {
                        println!(
                            "BLOCK 派发门：attempt {}#{} 待确认——{}",
                            gate.task_id,
                            gate.attempt_no,
                            gate.reason
                                .as_deref()
                                .unwrap_or("启动许可已授予，spawn/退出未确认")
                        );
                    }
                    for row in &blocking {
                        println!(
                            "BLOCK 受管组：attempt {}#{} 组 {}（组长 {}，启动 {}）——{}",
                            row.task_id,
                            row.attempt_no,
                            row.pgid,
                            row.leader_pid,
                            row.leader_start,
                            row.reason.as_deref().unwrap_or("未确认退出")
                        );
                    }
                    println!(
                        "      下一步：等待组退出（daemon 自动以可信退出证据解封）；\n      若进程已确认不存在，需人工确认后释放待确认标记（不放行则持续阻断）"
                    );
                }
            }
        }

        // 3. IPC socket 状态。
        let socket = instance.join("relay.sock");
        if socket.exists() {
            match std::os::unix::net::UnixStream::connect(&socket) {
                Ok(_) => println!("ok    IPC socket 活跃（有监听者）"),
                Err(e) if e.raw_os_error() == Some(libc::ECONNREFUSED) => {
                    let record = instance.join("relay.sock.own");
                    report(
                        record.exists(),
                        "IPC socket 为陈旧残留（无监听者）",
                        "有归属记录：下次启动自动接管；无记录：人工确认后移除",
                    );
                }
                Err(e) => report(
                    false,
                    "IPC socket 可判定",
                    &format!("连接错误 {e}——人工确认归属后处置"),
                ),
            }
        } else {
            println!("skip  IPC socket（不存在：daemon 未运行或未绑定）");
        }

        // 4. daemon.lock 可锁性（探测后立即释放；不影响在跑 daemon 之外
        //    的检查者）。
        let lock_path = instance.join("daemon.lock");
        if lock_path.exists() {
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path);
            match lock {
                Ok(f) => {
                    let try_lock = fs2::FileExt::try_lock_exclusive(&f);
                    match try_lock {
                        Ok(()) => {
                            let _ = fs2::FileExt::unlock(&f);
                            println!("ok    daemon.lock 无持有者（daemon 未运行）");
                        }
                        Err(_) => println!("ok    daemon.lock 被持有（daemon 运行中）"),
                    }
                }
                Err(e) => report(false, "daemon.lock 可读", &format!("读取失败：{e}")),
            }
        } else {
            println!("skip  daemon.lock（不存在：实例未启动过）");
        }

        if failures.get() > 0 {
            eprintln!("doctor：{} 项未通过", failures.get());
            Err(1)
        } else {
            println!("doctor：全部通过");
            Ok(())
        }
    }
}

/// relayctl mcp-verify：对 daemon 的 MCP 端点做真实握手 + 工具清单
/// 断言（T03 服务端门；真实 Codex 客户端发现另按文档待验）。
///
/// 通过判据：initialize 返回 2025-06-18、tools/list 返回固定顺序的
/// 8 个工具、响应可解析为 MCP 结构。失败退出码 1 并给出差异。
pub mod mcp_verify {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};

    const EXPECTED_TOOLS: [&str; 8] = [
        "relay_plan",
        "relay_submit",
        "relay_status",
        "relay_list",
        "relay_artifact",
        "relay_cancel",
        "relay_retry",
        "relay_review",
    ];

    pub fn run(args: &[String]) -> Result<(), u8> {
        let Some(socket) = flag(args, "--socket") else {
            eprintln!("缺少 --socket PATH");
            return Err(2);
        };
        // MCP 桥（04：MCP 以 relayctl mcp 的 stdio 运行）——验证对象是
        // 桥进程，而非裸 daemon socket（daemon 只讲 relay 协议）。
        let exe = std::env::current_exe().map_err(|e| {
            eprintln!("无法定位 relayctl：{e}");
            1u8
        })?;
        let mut child: Child = Command::new(exe)
            .arg("mcp")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                eprintln!("启动 MCP 桥失败：{e}");
                1u8
            })?;
        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut send = |value: &Value| -> Result<(), u8> {
            writeln!(stdin, "{value}").map_err(|_| {
                eprintln!("桥已关闭");
                1u8
            })
        };

        // 1. initialize（真实握手）。
        send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "relayctl-mcp-verify", "version": "m1"}},
        }))?;
        let init_line = read_line(&mut reader)?;
        let init: Value = serde_json::from_str(&init_line).map_err(|e| {
            eprintln!("initialize 响应不是 JSON：{e}");
            1u8
        })?;
        println!(
            "initialize protocolVersion = {}",
            init["result"]["protocolVersion"]
        );
        if init["result"]["protocolVersion"] != json!("2025-06-18") {
            eprintln!("FAIL：协议版本不是 2025-06-18");
            return Err(1);
        }

        // 2. notifications/initialized + tools/list（实际工具清单）。
        send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))?;
        let tools_line = read_line(&mut reader)?;
        let tools: Value = serde_json::from_str(&tools_line).map_err(|e| {
            eprintln!("tools/list 响应不是 JSON：{e}");
            1u8
        })?;
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
            .unwrap_or_default();
        println!("tools/list = {names:?}");
        if names != EXPECTED_TOOLS {
            eprintln!("FAIL：工具清单或顺序不符，预期 {EXPECTED_TOOLS:?}");
            return Err(1);
        }

        // 3. tools/call 往返：结构化结果或 isError 业务错误（M1 模式下
        // 不存在的任务返回 isError；骨架替身返回固定只读状态——两者都是
        // 合法 MCP 响应形状）。
        send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "relay_status", "arguments": {"task_id": "task_nonexistent"}}}))?;
        let call_line = read_line(&mut reader)?;
        let call: Value = serde_json::from_str(&call_line).map_err(|e| {
            eprintln!("tools/call 响应不是 JSON：{e}");
            1u8
        })?;
        let has_content = call["result"]["content"].is_array();
        let structured = call["result"]["structuredContent"].is_object();
        let is_error = call["result"]["isError"].as_bool();
        println!(
            "tools/call relay_status isError={} content_len={}",
            is_error.map_or_else(|| "?".into(), |value| value.to_string()),
            call["result"]["content"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0)
        );
        if !(has_content
            && structured
            && is_error == Some(call["result"]["structuredContent"]["error"].is_object()))
        {
            eprintln!("FAIL：tools/call 响应缺 content/structuredContent/isError 字段");
            return Err(1);
        }
        // 先关桥的 stdin（桥读到 EOF 即退出），再收割，避免互等。
        drop(send);
        drop(stdin);
        let _ = child.wait();
        println!("mcp-verify：握手、工具清单与调用往返全部通过");
        Ok(())
    }

    fn read_line(reader: &mut BufReader<std::process::ChildStdout>) -> Result<String, u8> {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| {
            eprintln!("读取响应失败：{e}");
            1u8
        })?;
        if line.is_empty() {
            eprintln!("FAIL：连接被关闭（无响应）");
            return Err(1);
        }
        Ok(line)
    }
}

/// relayctl adapter-check：M2 离线适配器样板（一条命令跑全部用例）。
/// 固定模拟输入；不启用真实模型、不读凭据、不发网络请求。Claude Code
/// + MiniMax 保持候选且默认禁用；启用所需依据随输出列出。
pub mod adapter_check {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn run(_args: &[String]) -> Result<(), u8> {
        let logic_cases = relay_daemon::adapter_sample::run_sample_cases();
        let mut chain_cases: Vec<(&'static str, bool, String)> = Vec::new();
        // (emit, actual_model, capabilities, expected_state)
        let outcomes = [
            (
                "success",
                "fake-adapter-v1",
                "read_files,edit_files",
                "awaiting_review",
            ),
            (
                "success",
                "some-other-model",
                "read_files,edit_files",
                "failed",
            ),
            ("success", "", "read_files,edit_files", "failed"),
            ("success", "fake-adapter-v1", "read_files,shell", "failed"),
            (
                "auth_failed",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "rate_limited",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "unsupported_protocol",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "invalid_json",
                "fake-adapter-v1",
                "read_files,edit_files",
                "awaiting_review",
            ),
            (
                "huge_frame",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "duplicate_terminal",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "conflicting_terminal",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "hold_stdout",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            (
                "alive_no_pipe",
                "fake-adapter-v1",
                "read_files,edit_files",
                "failed",
            ),
            ("hang", "fake-adapter-v1", "read_files,edit_files", "failed"),
        ];
        for (emit, model, caps, want_state) in outcomes {
            let dir = chain_env(emit, model, caps);
            let result = run_chain_case(&dir, emit, want_state);
            chain_cases.push((
                Box::leak(format!("{emit}/{model}/{caps}").into_boxed_str()),
                result.0,
                result.1,
            ));
            fs::remove_dir_all(&dir).ok();
        }
        let probe_hang = probe_hang_case();
        chain_cases.push((
            Box::leak("probe_hang".to_owned().into_boxed_str()),
            probe_hang.0,
            probe_hang.1,
        ));
        let report = json!({
            "schema_version": 1,
            "routes": relay_daemon::adapter_sample::CANDIDATE_ROUTES
                .iter()
                .map(|(id, enabled, requirements)| {
                    json!({"id": id, "enabled": enabled,
                            "status": if *enabled { "enabled" } else { "candidate-disabled" },
                            "requirements": requirements})
                })
                .collect::<Vec<_>>(),
            "logic_cases": relay_daemon::adapter_sample::cases_json(&logic_cases)["cases"],
            "chain_cases": chain_cases
                .iter()
                .map(|(name, pass, detail)| {
                    json!({"name": name, "pass": pass, "detail": detail})
                })
                .collect::<Vec<_>>(),
            "all_pass": chain_cases.iter().all(|(_, pass, _)| *pass)
                && logic_cases.iter().all(|(_, v, d)| {
                    matches!(v, relay_daemon::adapter_sample::Verdict::Pass)
                        || d.starts_with("PASS")
                }),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string())
        );
        if report["all_pass"] == json!(true) {
            println!("adapter-check：样板与链路用例全部通过（离线夹具；不证明真实权限/身份/套餐）");
            Ok(())
        } else {
            eprintln!("adapter-check：存在失败用例");
            Err(1)
        }
    }

    /// 链路用例环境：临时仓库 + 实例 + 配置（fixture 适配器）。
    fn chain_env(emit: &str, model: &str, caps: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "adchk-{emit}-{}-{}",
            std::process::id(),
            nanos % 1_000_000_000
        ));
        let repo = dir.join("repo");
        let instance = dir.join("instance");
        let managed = dir.join("managed");
        for path in [&repo, &instance, &managed] {
            fs::create_dir_all(path).unwrap();
        }
        fs::set_permissions(&instance, PermissionsExt::from_mode(0o700)).unwrap();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q", "-b", "main"])
            .output()
            .unwrap();
        assert!(output.status.success());
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(
            repo.join("src/count.rs"),
            "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
        )
        .unwrap();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "src/count.rs"])
            .env("GIT_AUTHOR_NAME", "adchk")
            .env("GIT_AUTHOR_EMAIL", "adchk@example.invalid")
            .output();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-q", "-m", "baseline"])
            .env("GIT_AUTHOR_NAME", "adchk")
            .env("GIT_AUTHOR_EMAIL", "adchk@example.invalid")
            .output();
        let rule = dir.join("rule.sh");
        fs::write(&rule, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&rule, PermissionsExt::from_mode(0o755)).unwrap();
        let config = serde_json::json!({
            "repo_root": repo.display().to_string(),
            "managed_root": managed.display().to_string(),
            "mock_worker": dir.join("unused.sh").display().to_string(),
            "adapter_fixture": PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("mock/mock_adapter.sh")
                .display()
                .to_string(),
            "expected_model": "fake-adapter-v1",
            "allowed_capabilities": ["read_files", "edit_files"],
            "adapter_actual_model": model,
            "adapter_capabilities": caps,
            "verification_rule_ids": ["mock_rule"],
            "worker_timeout_ms": 2_000,
            "adapter_probe_timeout_ms": 2_000,
            "adapter_read_timeout_ms": 2_000,
            "adapter_max_frame_bytes": 8 * 1024,
            "rules": [{"id": "mock_rule", "executable": rule.display().to_string(),
                        "argv": [], "timeout_ms": 5_000}],
            "adapter_emit": emit,
        });
        let config_path = dir.join("relay-config.json");
        fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
        dir
    }

    /// probe 挂起用例：probe 等待上限触发 → blocked（不进入启动）。
    fn probe_hang_case() -> (bool, String) {
        use std::sync::Arc;
        let dir = chain_env("success", "fake-adapter-v1", "read_files,edit_files");
        let instance = dir.join("instance");
        let config_text = fs::read_to_string(dir.join("relay-config.json")).unwrap();
        let config_value: Value = serde_json::from_str(&config_text).unwrap();
        let mut service_config =
            relay_daemon::service::ServiceConfig::from_json(&config_value).unwrap();
        service_config.adapter_probe_timeout_ms = 700;
        std::env::set_var("ADAPTER_PROBE_HANG", "1");
        std::env::set_var("ADAPTER_HOLD_SECONDS", "30");
        let store = Arc::new(relay_store::Store::open(&instance.join("relay.db")).unwrap());
        let service = relay_daemon::service::M1Service::new(store.clone(), service_config);
        let plan = service
            .dispatch(
                "relay_plan",
                Some(&serde_json::json!({
                    "project_id": "proj_adchk", "goal": "g", "brief": "b",
                    "write_scope": ["src/count.rs"], "profile_id": "prof_mock",
                    "verification_rule_ids": ["mock_rule"],
                })),
            )
            .unwrap();
        let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
        let task = service
            .dispatch(
                "relay_submit",
                Some(&serde_json::json!({
                    "plan_id": plan_id, "request_id": "ad7c6b2f-1e1e-4f8e-9a2b-3c4d5e6f7081",
                })),
            )
            .unwrap();
        let task_id = task["task"]["id"].as_str().unwrap().to_owned();
        let stop = AtomicBool::new(false);
        let started = std::time::Instant::now();
        service.run_one_claim(&stop);
        let elapsed = started.elapsed();
        let state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
        std::env::remove_var("ADAPTER_PROBE_HANG");
        std::env::remove_var("ADAPTER_HOLD_SECONDS");
        let ok = state == "failed" && elapsed < std::time::Duration::from_secs(8);
        fs::remove_dir_all(&dir).ok();
        (
            ok,
            format!(
                "probe 挂起：终态 {state}（预期 failed），耗时 {} ms（须在上限内）",
                elapsed.as_millis()
            ),
        )
    }

    /// 跑一个链路用例：in-process daemon（M1Service）+ 事件收敛判定。
    fn run_chain_case(dir: &Path, emit: &str, want_state: &str) -> (bool, String) {
        use relay_daemon::service::{M1Service, ServiceConfig};
        let instance = dir.join("instance");
        let config_text = fs::read_to_string(dir.join("relay-config.json")).unwrap();
        let config_value: Value = serde_json::from_str(&config_text).unwrap();
        let service_config = ServiceConfig::from_json(&config_value).unwrap();
        // 注入 ADAPTER_EMIT 环境变量由 fixture 读取：service spawn 继承
        // 本进程 env —— 用线程局部不可行，改为每用例独立进程内顺序执行。
        std::env::set_var("ADAPTER_EMIT", emit);
        std::env::set_var(
            "ADAPTER_ACTUAL_MODEL",
            config_value["adapter_actual_model"].as_str().unwrap_or(""),
        );
        std::env::set_var(
            "ADAPTER_CAPABILITIES",
            config_value["adapter_capabilities"].as_str().unwrap_or(""),
        );
        let store = Arc::new(relay_store::Store::open(&instance.join("relay.db")).unwrap());
        let service = M1Service::new(store.clone(), service_config);
        let plan = service
            .dispatch(
                "relay_plan",
                Some(&serde_json::json!({
                    "project_id": "proj_adchk", "goal": "g", "brief": "b",
                    "write_scope": ["src/count.rs"], "profile_id": "prof_mock",
                    "verification_rule_ids": ["mock_rule"],
                })),
            )
            .unwrap();
        let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
        let task = service
            .dispatch(
                "relay_submit",
                Some(&serde_json::json!({
                    "plan_id": plan_id, "request_id": "ad7c6b2f-1e1e-4f8e-9a2b-3c4d5e6f7081",
                })),
            )
            .unwrap();
        let task_id = task["task"]["id"].as_str().unwrap().to_owned();
        let stop = AtomicBool::new(false);
        service.run_one_claim(&stop);
        let mut state = String::new();
        for _ in 0..200 {
            state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
            if state == want_state {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        // 失败路径硬断言：不得登记任何制品（不产生可验收候选）。
        let artifact_clear =
            want_state != "failed" || store.get_artifact(&task_id, 1).unwrap().is_none();
        let ok = state == want_state && artifact_clear;
        let detail =
            format!("emit={emit} 终态 {state}（预期 {want_state}）；未登记制品={artifact_clear}");
        if !artifact_clear {
            eprintln!("FAIL {emit}: 失败路径登记了制品 {detail}");
        }
        (ok, detail)
    }
}

/// relayctl gate confirm：受管恢复操作（缺进程记录时的人工确认解封）。
/// 记录 attempt、证据与结果（审计）；无证据则保持阻断。
pub mod gate_cmd {
    use super::*;

    pub fn run(args: &[String]) -> Result<(), u8> {
        let Some(cmd) = args.first().map(String::as_str) else {
            eprintln!("用法: relayctl gate confirm --instance-dir <dir> --task-id <id> [--attempt N] --note <证据> [--by <操作者>] [--confirm-absent]");
            return Err(2);
        };
        if cmd != "confirm" {
            eprintln!("未知 gate 子命令 {cmd}");
            return Err(2);
        }
        let Some(instance) = flag(args, "--instance-dir").map(PathBuf::from) else {
            eprintln!("缺少 --instance-dir");
            return Err(2);
        };
        let Some(task_id) = flag(args, "--task-id") else {
            eprintln!("缺少 --task-id");
            return Err(2);
        };
        // --attempt 必须是正整数：非法值拒绝，不静默回退到 1。
        let attempt: u32 = match flag(args, "--attempt") {
            Some(text) => match text.parse::<u32>() {
                Ok(n) if n > 0 => n,
                _ => {
                    eprintln!("--attempt 必须是正整数（收到 {text:?}）");
                    return Err(2);
                }
            },
            None => 1,
        };
        let note = flag(args, "--note").unwrap_or_default();
        let operator = flag(args, "--by").unwrap_or_else(|| "operator".to_owned());
        let operator_confirmed = args.iter().any(|a| a == "--confirm-absent");
        // 离线维护入口：先按 daemon 相同的目录/文件/所有权规则取得实例
        // 独占锁，再打开现有数据库；活跃 daemon 持锁时直接拒绝（不打开
        // 库、不改待确认标记、不写审计）。锁持有到解封与审计提交完成。
        let (_instance_lock, _lock_path) = match relay_daemon::acquire_instance_lock(&instance) {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("gate confirm：无法取得实例独占锁（{err}）");
                return Err(1);
            }
        };
        let db = instance.join("relay.sqlite3");
        let store = match relay_store::Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("打开实例数据库失败：{e}");
                return Err(1);
            }
        };
        let service = relay_daemon::service::M1Service::new(
            std::sync::Arc::new(store),
            relay_daemon::service::ServiceConfig {
                project_binding: None,
                claude_review_fixture: None,
                opencode_review_fixture: None,
                live_routes: None,
                opencode_implement_fixture: None,
                claude_implement_fixture: None,
                repo_root: instance.clone(),
                managed_root: instance.clone(),
                mock_worker: instance.clone(),
                mock_worker_args: Vec::new(),
                rules: Vec::new(),
                stall_at: None,
                stall_file: None,
                reclaim_term_ms: 2_000,
                reclaim_kill_ms: 2_000,
                min_disk_free_bytes: 0,
                adapter_fixture: None,
                expected_model: None,
                allowed_capabilities: Vec::new(),
                adapter_probe_timeout_ms: 10_000,
                adapter_read_timeout_ms: 10_000,
                adapter_max_frame_bytes: 64 * 1024,
                group_exit_budget_ms: 2_000,
                worker_timeout_ms: 10_000,
                adapter_catalog_dir: None,
                workflow_template: relay_daemon::service::TEMPLATE_IMPLEMENT_REVIEW.to_owned(),
                max_rework: 1,
                implement_env: Default::default(),
                review_env: Default::default(),
            },
        );
        match service.confirm_gate_absence(&task_id, attempt, &operator, &note, operator_confirmed)
        {
            Ok(record) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": 1,
                        "task_id": record.task_id,
                        "attempt_no": record.attempt_no,
                        "evidence": record.evidence,
                        "result": record.result,
                        "resolved_by": record.resolved_by,
                        "resolved_at": record.resolved_at,
                    }))
                    .unwrap_or_default()
                );
                Ok(())
            }
            Err(err) => {
                eprintln!("{}", err.to_json());
                Err(1)
            }
        }
    }
}

/// relayctl demo-auto：首版离线演示（一条命令，固定样本）。
///
/// 临时 HOME/仓库/实例上，用两个不同的本地适配器跑通
/// 实现 → 本地验证 → 独立只读评审 → 可交付结果；提交后每次调用都是
/// 独立连接（前台不在线），链路自行完成；最后重启 daemon 验证不重复
/// 派发。场景（`--scenario`）：
///   rework-once   首轮被评审退回 → 自动返工一次 → 再验证 → 再评审通过（默认）
///   clean         一轮通过
///   always-reject 评审每次退回 → 到返工上限即停，不产生候选
///   rogue-write   评审试图写入候选 → 被检测并拦下
///   stale-digest  评审报告绑定旧补丁摘要 → 不能放行
/// 全部为离线假适配器，不启用真实模型、不发网络请求；结果不证明真实
/// 路线的权限、模型身份或套餐资格。`--keep` 保留实例供任务窗口打开。
pub mod demo_auto {
    use super::*;
    use relay_daemon::ipc::IpcConfig;
    use relay_daemon::service::ServiceConfig;
    use std::path::Path;
    use std::process::Command;

    struct Scenario {
        name: &'static str,
        implement_mode: Option<&'static str>,
        review_mode: Option<&'static str>,
        /// true = 预期进入待验收并可人工验收；false = 预期 failed 且无候选。
        delivers: bool,
        outline: &'static str,
    }

    const SCENARIOS: &[Scenario] = &[
        Scenario {
            name: "rework-once",
            implement_mode: Some("rework-once"),
            review_mode: None,
            delivers: true,
            outline: "首轮被评审退回 → 自动返工一次 → 再验证 → 再评审通过",
        },
        Scenario {
            name: "clean",
            implement_mode: None,
            review_mode: None,
            delivers: true,
            outline: "一轮即通过",
        },
        Scenario {
            name: "always-reject",
            implement_mode: Some("rework-once"),
            review_mode: Some("always-reject"),
            delivers: false,
            outline: "评审每次退回 → 到返工上限即停，不产生候选",
        },
        Scenario {
            name: "rogue-write",
            implement_mode: None,
            review_mode: Some("rogue-write"),
            delivers: false,
            outline: "评审试图写入受审候选 → 候选变化被检测，不放行",
        },
        Scenario {
            name: "stale-digest",
            implement_mode: None,
            review_mode: Some("stale-digest"),
            delivers: false,
            outline: "评审报告绑定旧补丁摘要 → 不能放行",
        },
    ];

    /// 离线适配器目录：`--catalog` > 环境变量 > 安装包布局 > 开发布局。
    fn resolve_catalog(args: &[String]) -> Result<PathBuf, u8> {
        if let Some(path) = flag(args, "--catalog") {
            return Ok(PathBuf::from(path));
        }
        if let Ok(path) = std::env::var("RELAY_ADAPTER_CATALOG") {
            return Ok(PathBuf::from(path));
        }
        if let Some(exe_dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            // 安装包布局：Contents/MacOS/relayctl → Contents/Resources/relay/mock/catalog
            let bundled = exe_dir.join("../Resources/relay/mock/catalog");
            if bundled.is_dir() {
                return Ok(bundled);
            }
        }
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/catalog");
        if dev.is_dir() {
            return Ok(dev);
        }
        eprintln!("找不到离线适配器目录：请用 --catalog <dir> 指定");
        Err(2)
    }

    fn git(root: &Path, args: &[&str]) -> Result<(), String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "relay-demo")
            .env("GIT_AUTHOR_EMAIL", "relay-demo@example.invalid")
            .env("GIT_COMMITTER_NAME", "relay-demo")
            .env("GIT_COMMITTER_EMAIL", "relay-demo@example.invalid")
            .output()
            .map_err(|e| e.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    fn head_of(repo: &Path) -> Result<String, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn call(socket: &Path, method: &str, params: Value) -> Result<Value, String> {
        client::call(socket, method, params)
    }

    pub fn run(args: &[String]) -> Result<(), u8> {
        let scenario_name = flag(args, "--scenario").unwrap_or_else(|| "rework-once".to_owned());
        let Some(scenario) = SCENARIOS.iter().find(|s| s.name == scenario_name) else {
            eprintln!(
                "未知场景 {scenario_name:?}；可选：{}",
                SCENARIOS
                    .iter()
                    .map(|s| s.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(2);
        };
        let work_dir = resolve_run_dir(args, "demo", "d")?;
        let keep = args.iter().any(|a| a == "--keep");
        let catalog = resolve_catalog(args)?;
        eprintln!("demo-auto[{}]：{}", scenario.name, scenario.outline);
        eprintln!("工作目录：{}", work_dir.display());

        let outcome = run_scenario(&work_dir, &catalog, scenario);
        let (summary, ok) = match outcome {
            Ok(summary) => (summary, true),
            Err(step) => (format!("失败：{step}"), false),
        };
        println!("{summary}");
        if !ok && !keep {
            let _ = std::fs::remove_dir_all(&work_dir);
        }
        if ok {
            println!(
                "demo-auto[{scenario_name}]：通过{}",
                if keep {
                    format!("；实例保留于 {}", work_dir.join("instance").display())
                } else {
                    String::new()
                }
            );
            Ok(())
        } else {
            eprintln!("demo-auto[{scenario_name}]：未通过");
            Err(1)
        }
    }

    fn run_scenario(
        work_dir: &Path,
        catalog: &Path,
        scenario: &Scenario,
    ) -> Result<String, String> {
        use std::os::unix::fs::PermissionsExt;
        let repo = work_dir.join("repo");
        let managed = work_dir.join("managed");
        let instance = work_dir.join("instance");
        for path in [&repo, &managed, &instance] {
            std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        }
        std::fs::set_permissions(&instance, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        git(&repo, &["init", "-q", "-b", "main"])?;
        std::fs::create_dir_all(repo.join("src")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(repo.join("tests")).map_err(|e| e.to_string())?;
        std::fs::write(
            repo.join("src/count.rs"),
            "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(repo.join("tests/count.rs"), "// placeholder\n")
            .map_err(|e| e.to_string())?;
        git(&repo, &["add", "src/count.rs", "tests/count.rs"])?;
        git(&repo, &["commit", "-q", "-m", "baseline"])?;
        let head = head_of(&repo)?;
        let rule = work_dir.join("rule.sh");
        std::fs::write(&rule, "#!/bin/sh\nexit 0\n").map_err(|e| e.to_string())?;
        std::fs::set_permissions(&rule, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let config_value = json!({
            "repo_root": repo.display().to_string(),
            "managed_root": managed.display().to_string(),
            "adapter_catalog_dir": catalog.display().to_string(),
            "worker_timeout_ms": 20_000,
            "rules": [{"id": "mock_rule", "executable": rule.display().to_string(),
                        "argv": [], "timeout_ms": 5_000}],
        });
        let mut config = ServiceConfig::from_json(&config_value)?;
        if let Some(mode) = scenario.implement_mode {
            config
                .implement_env
                .insert("IMPLEMENT_MODE".to_owned(), mode.to_owned());
        }
        if let Some(mode) = scenario.review_mode {
            config
                .review_env
                .insert("REVIEW_MODE".to_owned(), mode.to_owned());
        }
        // rogue-write：评审者拿到 worktree 内文件路径并尝试写入（daemon
        // 本不提供该路径）——先经进程内业务登记任务，拿到 task_id 再配。
        let mut task_id: Option<String> = None;
        if scenario.name == "rogue-write" {
            let store = std::sync::Arc::new(
                relay_store::Store::open(&instance.join("relay.sqlite3"))
                    .map_err(|e| e.to_string())?,
            );
            let preview = relay_daemon::service::M1Service::new(store, config.clone());
            let plan = preview
                .dispatch("relay_plan", Some(&json!({
                    "project_id": "proj_demo", "goal": "首版离线演示", "brief": "修改 src/count.rs 并经独立评审",
                    "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_auto",
                    "verification_rule_ids": ["mock_rule"], "base_oid": head,
                })))
                .ok_or("relay_plan 无响应")?;
            if let Some(error) = plan.get("error") {
                return Err(format!("relay_plan 失败：{error}"));
            }
            let submitted = preview
                .dispatch("relay_submit", Some(&json!({
                    "plan_id": plan["plan_id"], "request_id": "d3e00000-0000-4000-8000-000000000001",
                })))
                .ok_or("relay_submit 无响应")?;
            task_id = submitted["task"]["id"].as_str().map(str::to_owned);
            let target = managed
                .join("worktrees")
                .join(task_id.as_deref().unwrap_or_default())
                .join("attempt-1/src/count.rs");
            config.review_env.insert(
                "REVIEW_ROGUE_TARGET".to_owned(),
                target.to_string_lossy().to_string(),
            );
        }

        // 启动 daemon（后台）；每次调用是独立连接，前台不在线。
        let mut daemon =
            relay_daemon::start_m1(&instance, &now_text(), IpcConfig::default(), config.clone())
                .map_err(|e| format!("daemon 启动失败：{e}"))?;
        daemon.serve_background();
        let socket = daemon.socket_path().to_owned();
        let task_id = match task_id {
            Some(id) => id,
            None => {
                let plan = call(
                    &socket,
                    "relay_plan",
                    json!({
                        "project_id": "proj_demo", "goal": "首版离线演示", "brief": "修改 src/count.rs 并经独立评审",
                        "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_auto",
                        "verification_rule_ids": ["mock_rule"], "base_oid": head,
                    }),
                )?;
                if !plan["plan_id"].is_string() {
                    return Err(format!("relay_plan 未返回可提交 plan：{plan}"));
                }
                let task = call(
                    &socket,
                    "relay_submit",
                    json!({
                        "plan_id": plan["plan_id"], "request_id": "d3e00000-0000-4000-8000-000000000001",
                    }),
                )?;
                task["task"]["id"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("relay_submit 未建立任务：{task}"))?
            }
        };
        // 等链路自行完成（前台连接均已关闭）。
        let mut state = String::new();
        let mut revision = 0u64;
        for _ in 0..1_200 {
            let status = call(&socket, "relay_status", json!({"task_id": task_id}))?;
            state = status["task"]["state"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            revision = status["task"]["revision"].as_u64().unwrap_or(0);
            if state == "awaiting_review" || state == "failed" || state == "interrupted" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let events = call(
            &socket,
            "events.list",
            json!({"after_seq": 0, "limit": 100}),
        )?;
        let of_task = |kind: &str| -> usize {
            events["events"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter(|e| e["task_id"] == json!(task_id) && e["kind"] == json!(kind))
                        .count()
                })
                .unwrap_or(0)
        };
        let mut lines = Vec::new();
        lines.push(format!(
            "任务 {task_id}：终态 {state}（revision {revision}）"
        ));
        lines.push(format!(
            "轮次：实现 {} 次、验证 {} 次、评审 {} 次（退回 {} 次、通过 {} 次）",
            of_task("claim"),
            of_task("verify_round"),
            of_task("review_round"),
            of_task("review_changes_requested"),
            of_task("review_passed")
        ));
        let mut delivered = false;
        if state == "awaiting_review" {
            let handoff_text = call(
                &socket,
                "relay_artifact",
                json!({"task_id": task_id, "attempt_no": 1, "kind": "handoff"}),
            )?["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let handoff: Value = serde_json::from_str(&handoff_text).map_err(|e| e.to_string())?;
            let workflow = &handoff["workflow"];
            lines.push(format!(
                "评审结论：{}（返工 {} 次；实现者 {} / {}，评审者 {} / {}）",
                workflow["review"]["verdict"].as_str().unwrap_or_default(),
                workflow["reworks"],
                workflow["routing"]["implement"]["identity"]
                    .as_str()
                    .unwrap_or_default(),
                workflow["routing"]["implement"]["model"]
                    .as_str()
                    .unwrap_or_default(),
                workflow["routing"]["review"]["identity"]
                    .as_str()
                    .unwrap_or_default(),
                workflow["routing"]["review"]["model"]
                    .as_str()
                    .unwrap_or_default(),
            ));
            lines.push(format!(
                "  补丁摘要   {}",
                workflow["review"]["candidate_diff_sha256"]
                    .as_str()
                    .unwrap_or_default()
            ));
            lines.push(format!(
                "  收据摘要   {}",
                workflow["review"]["test_receipt_sha256"]
                    .as_str()
                    .unwrap_or_default()
            ));
            let patch = call(
                &socket,
                "relay_artifact",
                json!({"task_id": task_id, "attempt_no": 1, "kind": "diff", "limit": 2048}),
            )?["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let shown: Vec<&str> = patch.lines().take(12).collect();
            lines.push("交付差异（节选）：".to_owned());
            for line in shown {
                lines.push(format!("  {line}"));
            }
            // 人工验收：基于当前 revision 与两个摘要（模型评审不冒充用户）。
            let accepted = call(
                &socket,
                "relay_review",
                json!({
                    "task_id": task_id, "attempt_no": 1, "expected_revision": revision,
                    "request_id": "d3e00000-0000-4000-8000-000000000002", "verdict": "accept",
                    "diff_sha256": workflow["review"]["candidate_diff_sha256"],
                    "test_sha256": workflow["review"]["test_receipt_sha256"],
                }),
            );
            match accepted {
                Ok(result) => {
                    lines.push(format!(
                        "人工验收：{}",
                        result["task"]["state"].as_str().unwrap_or_default()
                    ));
                    delivered = true;
                }
                Err(e) => lines.push(format!("人工验收未完成：{e}")),
            }
        } else {
            let failed = events["events"]
                .as_array()
                .and_then(|list| {
                    list.iter()
                        .find(|e| {
                            e["task_id"] == json!(task_id) && e["kind"] == json!("workflow_failed")
                        })
                        .and_then(|e| e["reason_code"].as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            lines.push(format!("未交付：{failed}"));
            // 业务错误也以结果返回（带 error 字段）：失败路径不得读到已登记制品。
            let artifact = call(
                &socket,
                "relay_artifact",
                json!({"task_id": task_id, "attempt_no": 1, "kind": "diff"}),
            )?;
            if artifact.get("error").is_none() {
                return Err(format!("失败路径却读到了已登记制品：{artifact}"));
            }
            lines.push(format!(
                "候选不可见：{}",
                artifact["error"]["code"].as_str().unwrap_or_default()
            ));
        }
        // 重启 daemon：恢复不重派。
        drop(daemon);
        let restarted = relay_daemon::start_m1(
            &instance,
            "2026-11-23T01:00:00Z",
            IpcConfig::default(),
            config,
        )
        .map_err(|e| format!("重启失败：{e}"))?;
        let claims = relay_store::Store::open(&instance.join("relay.sqlite3"))
            .map_err(|e| e.to_string())?
            .list_events(0, 100)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| e.task_id == task_id && e.kind == "claim")
            .count();
        lines.push(format!("重启后派发次数：{claims}"));
        drop(restarted);
        if scenario.delivers != delivered {
            return Err(format!(
                "预期{}，实际终态 {state}",
                if scenario.delivers {
                    "交付待验收结果"
                } else {
                    "停止且不产生候选"
                }
            ));
        }
        if claims != 1 {
            return Err(format!("重启后派发次数应为 1，实际 {claims}"));
        }
        Ok(lines.join("\n"))
    }
}
