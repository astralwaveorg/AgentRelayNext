use super::*;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Command;

const ROOT_NAME: &str = ".agent-relay-next";
const PROJECT: &str = "proj_local_demo";

pub(super) fn run(args: &[String]) -> Result<(), u8> {
    let result = (|| -> Result<Value, String> {
        let home = std::env::var_os("HOME").ok_or("HOME 未设置")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("HOME 必须是绝对路径".into());
        }
        let root = home.join(ROOT_NAME);
        match args {
            [cmd] if cmd == "prepare" => {
                prepare(&root)?;
                Ok(json!({"instance_dir": root.join("instance")}))
            }
            [cmd] if cmd == "submit" => submit(&root),
            _ => Err("用法：relayctl local-preview <prepare|submit>".into()),
        }
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(message) => {
            eprintln!("{message}");
            Err(1)
        }
    }
}

fn prepare(root: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let catalog = exe.parent().ok_or("安装目录不存在")?.join("mock/catalog");
    if !catalog.is_dir() {
        return Err("缺少离线适配器，请用安装包内的 relayctl".into());
    }
    let private_dir = |path: &Path| {
        DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("无法新建 {}：{e}", path.display()))
    };
    // Fresh setup only. Subsequent installs must preserve data and migrate explicitly.
    private_dir(root)?;
    let repo = root.join("repo");
    let managed = root.join("managed");
    let instance = root.join("instance");
    for dir in [
        &repo,
        &managed,
        &instance,
        &repo.join("src"),
        &repo.join("tests"),
    ] {
        private_dir(dir)?;
    }
    fs::write(
        repo.join("src/count.rs"),
        "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
    )
    .map_err(|e| e.to_string())?;
    fs::write(repo.join("tests/count.rs"), "// offline sample\n").map_err(|e| e.to_string())?;
    git(&repo, &["init", "-q", "-b", "main", "--template="])?;
    git(&repo, &["add", "--", "src/count.rs", "tests/count.rs"])?;
    git(&repo, &["commit", "-q", "-m", "Offline sample"])?;
    let head = git(&repo, &["rev-parse", "HEAD"])?;
    fs::write(root.join("baseline"), head.trim()).map_err(|e| e.to_string())?;
    let rule = root.join("verify-demo.sh");
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&rule)
        .and_then(|mut f| {
            f.write_all(b"#!/bin/sh\nexec /usr/bin/grep -q 'pub fn count' src/count.rs\n")
        })
        .map_err(|e| e.to_string())?;
    let config = json!({
        "repo_root": repo,
        "managed_root": managed,
        "adapter_catalog_dir": catalog,
        "implement_env": {"IMPLEMENT_MODE":"rework-once"},
        "worker_timeout_ms": 20000,
        "rules": [{"id":"mock_rule","executable":rule,"argv":[],"timeout_ms":5000}]
    });
    relay_daemon::service::ServiceConfig::from_json(&config)?;
    fs::write(root.join("config.json"), config.to_string()).map_err(|e| e.to_string())
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Agent Relay Demo",
            "-c",
            "user.email=local@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "无法准备演示仓库：{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

fn submit(root: &Path) -> Result<Value, String> {
    let base_oid =
        fs::read_to_string(root.join("baseline")).map_err(|e| format!("本机演示未就绪：{e}"))?;
    let socket = root.join("instance/relay.sock");
    let plan = client::call(
        &socket,
        "relay_plan",
        json!({
            "project_id": PROJECT,
            "goal":"离线样本：实现、测试与独立评审",
            "brief":"修改示例计数函数，评审退回后返工一次",
            "write_scope":["src/count.rs","tests/count.rs"],
            "profile_id":"prof_auto", "verification_rule_ids":["mock_rule"],
            "base_oid":base_oid.trim()
        }),
    )?;
    let plan_id = plan["plan_id"]
        .as_str()
        .ok_or_else(|| format!("计划未就绪：{plan}"))?;
    let request_id = Command::new("/usr/bin/uuidgen")
        .output()
        .map_err(|e| e.to_string())?;
    if !request_id.status.success() {
        return Err("无法生成请求 ID".into());
    }
    client::call(
        &socket,
        "relay_submit",
        json!({
            "plan_id":plan_id,
            "request_id":String::from_utf8_lossy(&request_id.stdout).trim().to_ascii_lowercase()
        }),
    )
}
