//! Local project registration. Captures an explicitly selected repo without starting a route.
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    name: String,
    repo_root: PathBuf,
    write_scope: Vec<String>,
    rule: Rule,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    id: String,
    executable: PathBuf,
    argv: Vec<String>,
    timeout_ms: u64,
}

fn preview(draft: &Draft) -> Result<Value, String> {
    if draft.name.trim().is_empty()
        || draft.name.len() > 128
        || draft.name.chars().any(char::is_control)
    {
        return Err("项目名称必须在 1–128 字节以内，不能含控制字符".into());
    }
    if !draft.repo_root.is_absolute() {
        return Err("请选择绝对路径的本机 Git 仓库".into());
    }
    let repo = draft
        .repo_root
        .canonicalize()
        .map_err(|e| format!("仓库路径不可用：{e}"))?;
    let fs_ident = |path: &Path| -> Result<Value, String> {
        let m =
            fs::symlink_metadata(path).map_err(|e| format!("不是可用的 Git 主 checkout：{e}"))?;
        if !m.is_dir() {
            return Err("请选择含真实 .git 目录的 Git 主 checkout".into());
        }
        Ok(json!([m.dev(), m.ino()]))
    };
    let identity = [fs_ident(&repo)?, fs_ident(&repo.join(".git"))?];
    let head = relay_git::read_repository_head(&repo).map_err(|e| e.to_string())?;
    relay_git::verify_baseline(&repo, &head).map_err(|e| e.to_string())?;
    let scope = draft
        .write_scope
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if scope.is_empty() {
        return Err("至少选择一项写入范围".into());
    }
    relay_git::WriteScope::new(&scope).map_err(|e| e.to_string())?;
    let rule = &draft.rule;
    let valid_exec = rule.executable.is_absolute()
        && fs::metadata(&rule.executable).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0);
    if !valid_exec
        || !(1..=40).contains(&rule.id.len())
        || !rule
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || !(100..=600_000).contains(&rule.timeout_ms)
        || rule.argv.len() > 32
        || rule
            .argv
            .iter()
            .any(|a| a.len() > 1024 || a.chars().any(char::is_control))
    {
        return Err(
            "验证规则须包含有效 ID、可执行的绝对路径、逐项参数和 100–600000 ms 超时".into(),
        );
    }
    let root_bytes = repo.as_os_str().as_encoded_bytes();
    let id = format!("proj_{}", &relay_git::sha256_hex(root_bytes)[..20]);
    let registration = json!({"schema_version":1,"name":draft.name.trim(),"repo_root":repo,
        "repository_identity":identity,"base_at_registration":head,
        "project_binding":{"project_id":id,"write_scope":draft.write_scope},
        "rules":[rule],"ready_for_dispatch":false});
    let bytes = serde_json::to_vec(&registration).map_err(|e| e.to_string())?;
    Ok(json!({"approval":relay_git::sha256_hex(&bytes), "registration":registration}))
}

fn check_private_directory(path: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !m.is_dir() || m.mode() & 0o7777 != 0o700 || m.uid() != rustix::process::geteuid().as_raw() {
        return Err("登记目录须为当前用户所有的真实 0700 目录".into());
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    check_private_directory(path)
}

fn save(home: &Path, checked: &Value) -> Result<Value, String> {
    if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(home) {
        return Err("HOME 须为真实的绝对路径".into());
    }
    let root = home.join(".agent-relay-next");
    let registry = root.join("projects");
    let repo = Path::new(
        checked["registration"]["repo_root"]
            .as_str()
            .ok_or("路径无法编码")?,
    );
    if registry.starts_with(repo) {
        return Err("登记资料不能写入待登记的仓库".into());
    }
    ensure_private_directory(&root)?;
    ensure_private_directory(&registry)?;
    let id = checked["registration"]["project_binding"]["project_id"]
        .as_str()
        .ok_or("缺少项目 ID")?;
    let dest = registry.join(id);
    let staging = registry.join(format!(".new-{}", uuid_hex()?));
    DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|e| e.to_string())?;
    let outcome = (|| {
        let text =
            serde_json::to_vec_pretty(&checked["registration"]).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(staging.join("registration.json"))
            .map_err(|e| e.to_string())?;
        file.write_all(&text)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(&staging)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        relay_sys::rename_no_replace(&staging, &dest).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                "项目已经登记，保留已有资料".into()
            } else {
                e.to_string()
            }
        })?;
        File::open(&registry)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(json!({"path":dest.join("registration.json"),"registration":checked["registration"]}))
    })();
    if outcome.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    outcome
}
fn uuid_hex() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(relay_git::sha256_hex(&bytes)[..24].into())
}

fn registry() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("未配置 HOME")?;
    if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(&home) {
        return Err("HOME 须为真实的绝对路径".into());
    }
    Ok(home.join(".agent-relay-next/projects"))
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("proj_").is_some_and(|s| {
        s.len() == 20
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn read_registration(registry: &Path, id: &str) -> Result<Value, String> {
    if !valid_id(id) {
        return Err("项目 ID 无效".into());
    }
    let directory = registry.join(id);
    check_private_directory(&directory)?;
    // NONBLOCK prevents a FIFO at this path from blocking before the type check.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join("registration.json"))
        .map_err(|e| e.to_string())?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file()
        || m.mode() & 0o7777 != 0o600
        || m.nlink() != 1
        || m.uid() != rustix::process::geteuid().as_raw()
    {
        return Err("登记文件必须是当前用户所有的 0600 常规文件".into());
    }
    let mut text = String::new();
    file.take(65537)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 65536 {
        return Err("登记文件超过 64 KiB".into());
    }
    let record = relay_protocol::frame::decode_json_value(&text).map_err(|e| e.to_string())?;
    if record["schema_version"] != 1
        || record["ready_for_dispatch"] != false
        || record["project_binding"]["project_id"] != id
    {
        return Err("登记版本、项目 ID 或执行状态无效".into());
    }
    Ok(record)
}

fn list() -> Result<Value, String> {
    let registry = registry()?;
    for path in [
        registry.parent().expect("registry parent"),
        registry.as_path(),
    ] {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"projects": []}))
            }
            _ => check_private_directory(path)?,
        }
    }
    let mut projects = Vec::new();
    let mut unavailable = 0;
    for entry in fs::read_dir(&registry).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|name| valid_id(name)) else {
            continue;
        };
        match read_registration(&registry, id) {
            Ok(record) if record["name"].is_string() => {
                projects.push(json!({"id":id,"name":record["name"]}));
            }
            _ => unavailable += 1,
        }
    }
    projects.sort_by_key(|p| p["id"].as_str().unwrap_or_default().to_owned());
    Ok(json!({"projects":projects, "unavailable":unavailable}))
}

struct Registered {
    record: Value,
    instance: PathBuf,
    managed: PathBuf,
    config: relay_daemon::service::ServiceConfig,
}

fn registered(id: &str) -> Result<Registered, String> {
    let registry = registry()?;
    check_private_directory(registry.parent().expect("registry parent"))?;
    check_private_directory(&registry)?;
    let record = read_registration(&registry, id)?;
    let rules = record["rules"]
        .as_array()
        .filter(|r| r.len() == 1)
        .ok_or("登记中缺少验证规则")?;
    let draft = serde_json::from_value::<Draft>(json!({
        "name":record["name"], "repo_root":record["repo_root"],
        "write_scope":record["project_binding"]["write_scope"], "rule":rules[0]
    }))
    .map_err(|e| e.to_string())?;
    let current = preview(&draft)?;
    let checked = &current["registration"];
    if checked["repository_identity"] != record["repository_identity"]
        || checked["repo_root"] != record["repo_root"]
        || checked["project_binding"] != record["project_binding"]
    {
        return Err("登记的仓库位置或目录身份已改变".into());
    }
    let directory = registry.join(id);
    if directory.starts_with(&draft.repo_root) {
        return Err("实例不能写入登记的仓库".into());
    }
    let instance = directory.join("instance");
    let managed = directory.join("managed");
    let config = relay_daemon::service::ServiceConfig::from_json(&json!({
        "repo_root":checked["repo_root"], "managed_root":managed,
        "project_binding":checked["project_binding"], "rules":checked["rules"]
    }))?;
    Ok(Registered {
        record,
        instance,
        managed,
        config,
    })
}

fn serve(id: &str) -> Result<(), String> {
    let Registered {
        instance,
        managed,
        config,
        ..
    } = registered(id)?;
    ensure_private_directory(&instance)?;
    ensure_private_directory(&managed)?;
    let mut daemon = relay_daemon::start_project_inspection(&instance, &super::now_text(), config)
        .map_err(|e| e.to_string())?;
    daemon.serve_background();
    let mut output = std::io::stdout().lock();
    writeln!(
        output,
        "{}",
        json!({"socket":daemon.socket_path(),"project_id":id})
    )
    .and_then(|_| output.flush())
    .map_err(|e| e.to_string())?;
    // GUI keeps the write end open. EOF (including app crash/quit) drops the daemon.
    let _ = std::io::stdin()
        .lock()
        .read(&mut [0u8; 1])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn route_setup(
    id: &str,
    claude: &str,
    opencode: &str,
) -> Result<(Registered, relay_daemon::live_routes::RoutePairSpec, Value), String> {
    let entry = registered(id)?;
    let route =
        relay_daemon::live_routes::RoutePairSpec::inspect(Path::new(claude), Path::new(opencode))
            .map_err(str::to_owned)?;
    let details = json!({
        "project_id":id, "name":entry.record["name"], "repo_root":entry.record["repo_root"],
        "repository_identity":entry.record["repository_identity"],
        "write_scope":entry.record["project_binding"]["write_scope"],
        "verification_rules":entry.record["rules"],
        "route":route.disclosure(), "needs_per_task_confirmation":true,
    });
    let digest = relay_git::sha256_hex(&serde_json::to_vec(&details).map_err(|e| e.to_string())?);
    Ok((entry, route, json!({"approval":digest,"details":details})))
}

fn serve_live(id: &str, claude: &str, opencode: &str, approval: &str) -> Result<(), String> {
    let (entry, route, preview) = route_setup(id, claude, opencode)?;
    if preview["approval"] != approval {
        return Err("项目、CLI 或路线与批准内容不符，请重新预览".into());
    }
    let Registered {
        instance,
        managed,
        mut config,
        ..
    } = entry;
    // Parsing credentials doesn't open a network connection. The worker only
    // sees them once the matching project plan is confirmed and fenced.
    config.attach_live_routes(route.read_authorized_keys().map_err(str::to_owned)?)?;
    ensure_private_directory(&instance)?;
    ensure_private_directory(&managed)?;
    relay_sys::install_stop_signals().map_err(|e| format!("注册停止信号失败：{e}"))?;
    let mut daemon = relay_daemon::start_m1(
        &instance,
        &super::now_text(),
        relay_daemon::ipc::IpcConfig::default(),
        config,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "{}",
        json!({"socket":daemon.socket_path(),"project_id":id,"dispatch_enabled":true})
    );
    daemon.serve_until(relay_sys::stop_requested);
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<(), u8> {
    if let [op, flag, id] = args {
        if op == "serve" && flag == "--project-id" {
            return serve(id).map_err(|error| {
                eprintln!("{error}");
                1
            });
        }
    }
    if let [op, id, claude, opencode, flag, approval, credits, extra] = args {
        if op == "serve-live"
            && flag == "--approve"
            && credits == "--allow-plan-credits"
            && extra == "--extra-usage-off"
        {
            return serve_live(id, claude, opencode, approval).map_err(|error| {
                eprintln!("{error}");
                1
            });
        }
    }
    let operation = (|| -> Result<Value, String> {
        if let [op, id, claude, opencode] = args {
            if op == "route-preview" {
                return route_setup(id, claude, opencode).map(|(_, _, preview)| preview);
            }
        }
        if matches!(args, [op] if op == "list") {
            return list();
        }
        if !matches!(args, [a] if a == "preview")
            && !matches!(args, [a, flag, _] if a == "save" && flag == "--approve")
        {
            return Err(
                "用法：relayctl project-setup preview|save --approve <digest>（JSON 经 stdin）；route-preview <id> <claude> <opencode>；serve-live <id> <claude> <opencode> --approve <digest> --allow-plan-credits --extra-usage-off"
                    .into(),
            );
        }
        let mut input = String::new();
        std::io::stdin()
            .lock()
            .take(65537)
            .read_to_string(&mut input)
            .map_err(|e| e.to_string())?;
        if input.len() > 65536 {
            return Err("登记请求过长".into());
        }
        let value = relay_protocol::frame::decode_json_value(&input).map_err(|e| e.to_string())?;
        let draft = serde_json::from_value::<Draft>(value).map_err(|e| e.to_string())?;
        let checked = preview(&draft)?;
        if args[0] == "preview" {
            return Ok(checked);
        }
        if checked["approval"] != args[2].as_str() {
            return Err("登记信息或 Git 基线已改变，请重新预览".into());
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("未配置 HOME")?;
        save(&home, &checked)
    })();
    match operation {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(error) => {
            eprintln!("{error}");
            Err(1)
        }
    }
}
