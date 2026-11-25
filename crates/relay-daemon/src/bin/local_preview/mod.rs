use super::*;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const ROOT_NAME: &str = ".agent-relay-next";
const PROJECT: &str = "proj_local_demo";
const DEFAULT_LABEL: &str = "net.relay.candidate.local-preview";
const SUN_LEN: usize = 104;

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
            [cmd] if cmd == "enable" => enable(&home, &root),
            [cmd] if cmd == "disable" => disable(&home, &root),
            [cmd] if cmd == "status" => status(&home, &root),
            _ => Err("用法：relayctl local-preview <prepare|enable|status|disable|submit>".into()),
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

fn label() -> String {
    std::env::var("RELAY_PREVIEW_LABEL").unwrap_or_else(|_| DEFAULT_LABEL.to_owned())
}

fn validate_label(label: &str) -> Result<(), String> {
    if !label.starts_with(DEFAULT_LABEL)
        || label
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
    {
        return Err("LaunchAgent label 不匹配 Relay 试用服务".into());
    }
    Ok(())
}

fn socket_path(root: &Path) -> PathBuf {
    root.join("instance/relay.sock")
}

fn validate_socket(root: &Path) -> Result<(), String> {
    let socket = socket_path(root);
    if socket.to_string_lossy().len() >= SUN_LEN {
        return Err(format!(
            "socket 路径超过 SUN_LEN 限制：{}",
            socket.display()
        ));
    }
    Ok(())
}

fn plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

fn session() -> Result<String, String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|e| format!("无法读取当前 UID：{e}"))?;
    if !output.status.success() {
        return Err("无法读取当前 UID".into());
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
        return Err("UID 输出异常".into());
    }
    Ok(format!("gui/{uid}"))
}

/// 校验关键路径均为物理路径（不跟随符号链接）。
fn physical(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(format!("拒绝符号链接：{}", path.display()))
        }
        _ => Ok(()),
    }
}

fn verify_existing(root: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(root).map_err(|e| format!("无法读取 {}：{e}", root.display()))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(format!("{} 不是常规实例目录", root.display()));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        return Err("实例目录权限必须为 0700".into());
    }
    for name in ["config.json", "baseline"] {
        let path = root.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("缺少 {name}：{e}"))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(format!("{name} 必须是常规文件"));
        }
    }
    let baseline =
        fs::read_to_string(root.join("baseline")).map_err(|e| format!("无法读取 baseline：{e}"))?;
    let baseline = baseline.trim();
    if !matches!(baseline.len(), 40 | 64)
        || !baseline
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("baseline 不是完整小写 commit OID".into());
    }
    if !root.join("instance").is_dir() {
        return Err("实例目录缺少 instance/".into());
    }
    Ok(())
}

fn install_wrapper(root: &Path, source: &Path) -> Result<(), String> {
    let target = root.join("run-daemon.sh");
    physical(&target)?;
    fs::copy(source, &target).map_err(|e| format!("无法安装 run-daemon.sh：{e}"))?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("无法设置 run-daemon.sh 权限：{e}"))
}

fn plist_insert(file: &Path, key: &str, value: &str) -> Result<(), String> {
    let ok = Command::new("/usr/bin/plutil")
        .args([
            "-insert",
            key,
            "-string",
            value,
            file.to_str().ok_or("路径不是 UTF-8")?,
        ])
        .status()
        .map_err(|e| format!("plutil 失败：{e}"))?;
    if ok.success() {
        Ok(())
    } else {
        Err(format!("plutil 无法写入 {key}"))
    }
}

fn write_plist(home: &Path, label: &str, args: &[String]) -> Result<PathBuf, String> {
    let dir = home.join("Library/LaunchAgents");
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建 LaunchAgents 目录：{e}"))?;
    physical(&dir)?;
    let temp = dir.join(format!(".relay-enable.{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    let temp_str = temp.to_str().ok_or("临时 plist 路径不是 UTF-8")?;
    let ok = Command::new("/usr/bin/plutil")
        .args(["-create", "xml1", temp_str])
        .status()
        .map_err(|e| format!("plutil 失败：{e}"))?;
    if !ok.success() {
        return Err("无法创建 plist".into());
    }
    plist_insert(&temp, "Label", label)?;
    let ok = Command::new("/usr/bin/plutil")
        .args(["-insert", "ProgramArguments", "-json", "[]", temp_str])
        .status()
        .map_err(|e| format!("plutil 失败：{e}"))?;
    if !ok.success() {
        return Err("无法初始化 ProgramArguments".into());
    }
    for (index, arg) in args.iter().enumerate() {
        plist_insert(&temp, &format!("ProgramArguments.{index}"), arg)?;
    }
    for key in ["RunAtLoad", "KeepAlive"] {
        let ok = Command::new("/usr/bin/plutil")
            .args(["-insert", key, "-bool", "true", temp_str])
            .status()
            .map_err(|e| format!("plutil 失败：{e}"))?;
        if !ok.success() {
            return Err(format!("无法写入 {key}"));
        }
    }
    let ok = Command::new("/usr/bin/plutil")
        .args(["-lint", temp_str])
        .output()
        .map_err(|e| format!("plutil 失败：{e}"))?;
    if !ok.status.success() {
        let _ = fs::remove_file(&temp);
        return Err("生成的 plist 未通过 plutil 校验".into());
    }
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("无法设置 plist 权限：{e}"))?;
    let target = plist_path(home, label);
    physical(&target)?;
    fs::rename(&temp, &target).map_err(|e| format!("无法原子写入 plist：{e}"))?;
    Ok(target)
}

fn read_plist(file: &Path, key: &str) -> Result<String, String> {
    let output = Command::new("/usr/bin/plutil")
        .args([
            "-extract",
            key,
            "raw",
            "-o",
            "-",
            file.to_str().ok_or("路径不是 UTF-8")?,
        ])
        .output()
        .map_err(|e| format!("plutil 失败：{e}"))?;
    if !output.status.success() {
        return Err(format!("plist 缺少 {key}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn launchctl_print(session: &str, label: &str) -> bool {
    Command::new("launchctl")
        .args(["print", &format!("{session}/{label}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn launchctl(action: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new("launchctl")
        .arg(action)
        .args(args)
        .status()
        .map_err(|e| format!("launchctl {action} 失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("launchctl {action} 返回失败"))
    }
}

fn enable(home: &Path, root: &Path) -> Result<Value, String> {
    let label = label();
    validate_label(&label)?;
    physical(root)?;
    validate_socket(root)?;
    let exe = std::env::current_exe().map_err(|e| format!("无法定位 relayctl：{e}"))?;
    let exe = exe
        .canonicalize()
        .map_err(|e| format!("无法解析 relayctl 路径：{e}"))?;
    let dir = exe.parent().ok_or("安装目录不存在")?;
    for name in ["relayctl", "run-daemon.sh"] {
        physical(&dir.join(name))?;
    }
    if !dir.join("mock/catalog").is_dir() {
        return Err("缺少离线适配器，请使用安装包内的 relayctl".into());
    }
    physical(&home.join("Library/LaunchAgents"))?;
    physical(&home.join("Library"))?;
    if !root.exists() {
        prepare(root)?;
    } else {
        verify_existing(root)?;
    }
    let wrapper = dir.join("run-daemon.sh");
    install_wrapper(root, &wrapper)?;
    let args = vec![
        root.join("run-daemon.sh").to_string_lossy().into_owned(),
        root.to_string_lossy().into_owned(),
        root.join("instance").to_string_lossy().into_owned(),
        root.join("config.json").to_string_lossy().into_owned(),
        exe.to_string_lossy().into_owned(),
    ];
    let plist = write_plist(home, &label, &args)?;
    let session = session()?;
    if launchctl_print(&session, &label) {
        let _ = launchctl("bootout", &[&format!("{session}/{label}")]);
    }
    launchctl(
        "bootstrap",
        &[
            session.as_str(),
            plist.to_str().ok_or("plist 路径不是 UTF-8")?,
        ],
    )?;
    let socket = socket_path(root);
    let mut ready = false;
    for _ in 0..80 {
        if socket.exists() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !ready {
        let _ = launchctl("bootout", &[&format!("{session}/{label}")]);
        let _ = fs::remove_file(&plist);
        return Err("服务未在期限内就绪；已撤销本次 plist，保留实例目录供诊断".into());
    }
    let instance = root.join("instance");
    doctor(&instance)?;
    mcp_verify(&socket)?;
    Ok(json!({
        "status": "running",
        "instance_dir": instance,
        "socket": socket,
        "service_label": label,
        "data_preserved": true
    }))
}

fn doctor(instance: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = Command::new(exe)
        .args(["doctor", "--instance-dir"])
        .arg(instance)
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("doctor 启动失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("doctor 未通过".into())
    }
}

fn mcp_verify(socket: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = Command::new(exe)
        .args(["mcp-verify", "--socket"])
        .arg(socket)
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("mcp-verify 启动失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("MCP 握手未通过".into())
    }
}

fn disable(home: &Path, root: &Path) -> Result<Value, String> {
    let label = label();
    validate_label(&label)?;
    let plist = plist_path(home, &label);
    if !plist.is_file() {
        return Ok(json!({"status": "not_configured", "data_preserved": true}));
    }
    physical(&plist)?;
    if read_plist(&plist, "Label")? != label {
        return Err("plist Label 不属于 Relay，拒绝操作".into());
    }
    let first = read_plist(&plist, "ProgramArguments.0")?;
    let wrapper = root.join("run-daemon.sh");
    if Path::new(&first) != wrapper {
        return Err("plist 入口不属于 Relay，拒绝操作".into());
    }
    let session = session()?;
    let _ = launchctl("bootout", &[&format!("{session}/{label}")]);
    let socket = socket_path(root);
    for _ in 0..100 {
        if !socket.exists() && !launchctl_print(&session, &label) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    fs::remove_file(&plist).map_err(|e| format!("无法移除 plist：{e}"))?;
    Ok(json!({"status": "configured_not_running", "data_preserved": true}))
}

fn status(home: &Path, root: &Path) -> Result<Value, String> {
    let label = label();
    validate_label(&label)?;
    let plist = plist_path(home, &label);
    let socket = socket_path(root);
    if !root.join("config.json").is_file() {
        return Ok(json!({"status": "not_configured", "instance_dir": root}));
    }
    if !plist.is_file() {
        return Ok(json!({"status": "configured_not_running", "instance_dir": root}));
    }
    let session = session()?;
    if !launchctl_print(&session, &label) {
        return Ok(json!({"status": "configured_not_running", "instance_dir": root}));
    }
    if !socket.exists() {
        return Ok(json!({"status": "starting", "instance_dir": root}));
    }
    let healthy =
        doctor(&root.join("instance")).is_ok() && mcp_verify(socket_path(root).as_path()).is_ok();
    Ok(json!({
        "status": if healthy { "running" } else { "unhealthy" },
        "instance_dir": root,
        "socket": socket,
        "service_label": label
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_and_socket_validation() {
        assert!(validate_label("net.relay.candidate.local-preview").is_ok());
        assert!(validate_label("net.relay.candidate.local-preview.qa.1").is_ok());
        assert!(validate_label("com.example.other").is_err());
        assert!(validate_label("net.relay.candidate.local-preview;rm").is_err());
        assert!(validate_socket(Path::new("/private/tmp/rr")).is_ok());
        let long = PathBuf::from(format!("/private/tmp/{}", "a".repeat(120)));
        assert!(validate_socket(&long).is_err());
        assert_eq!(
            plist_path(
                Path::new("/Users/test"),
                "net.relay.candidate.local-preview"
            ),
            PathBuf::from(
                "/Users/test/Library/LaunchAgents/net.relay.candidate.local-preview.plist"
            )
        );
    }
}
