//! M1 收尾契约测试：强杀注入矩阵（08 T08 的可重复等价物，与真实断电
//! 测试分开记录）、TERM 顽固子进程升级回收、陈旧 socket 受管接管、
//! 备份恢复后制品映射（T11）与异常结果分类（T09 部分）。

use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relay_daemon::service::{M1Service, ServiceConfig};
use relay_store::Store;

fn temp_root(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let base = if Path::new("/private/tmp").is_dir() {
        std::path::PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!(
        "m1rec-{name}-{}{seq}{}",
        std::process::id(),
        nanos % 1_000_000
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "m1rec")
        .env("GIT_AUTHOR_EMAIL", "m1rec@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} 失败");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Env {
    dir: PathBuf,
    config: ServiceConfig,
    head: String,
}

fn setup(name: &str, worker_arg: Option<&str>) -> Env {
    let dir = temp_root(name);
    let repo = dir.join("repo");
    let managed = dir.join("managed");
    let instance = dir.join("instance");
    for path in [&repo, &managed, &instance] {
        fs::create_dir_all(path).unwrap();
    }
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(
        repo.join("src/count.rs"),
        "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
    )
    .unwrap();
    fs::write(repo.join("tests/count.rs"), "// placeholder\n").unwrap();
    git(&repo, &["add", "src/count.rs", "tests/count.rs"]);
    git(&repo, &["commit", "-q", "-m", "baseline"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let rule = dir.join("rule.sh");
    fs::write(&rule, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&rule, fs::Permissions::from_mode(0o755)).unwrap();
    let config = ServiceConfig {
        project_binding: None,
        claude_review_fixture: None,
        opencode_review_fixture: None,
        live_routes: None,
        opencode_implement_fixture: None,
        claude_implement_fixture: None,
        repo_root: repo,
        managed_root: managed,
        mock_worker: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/mock_worker.sh"),
        mock_worker_args: worker_arg.map(|a| vec![a.to_owned()]).unwrap_or_default(),
        rules: vec![relay_git::VerifyRule {
            id: "mock_rule".into(),
            executable: rule.to_string_lossy().to_string(),
            argv: vec![],
            timeout_ms: 5_000,
        }],
        stall_at: None,
        stall_file: None,
        min_disk_free_bytes: 0,
        adapter_fixture: None,
        expected_model: None,
        allowed_capabilities: vec!["read_files".to_owned(), "edit_files".to_owned()],
        adapter_probe_timeout_ms: 10_000,
        adapter_read_timeout_ms: 10_000,
        adapter_max_frame_bytes: 64 * 1024,
        group_exit_budget_ms: 2_000,
        adapter_catalog_dir: None,
        workflow_template: "implement_review".to_owned(),
        max_rework: 1,
        implement_env: Default::default(),
        review_env: Default::default(),
        reclaim_term_ms: 2_000,
        reclaim_kill_ms: 2_000,
        worker_timeout_ms: 20_000,
    };
    Env { dir, config, head }
}

fn config_json(env: &Env, stall: Option<&str>, marker: Option<&Path>) -> Value {
    use serde_json::json;
    let mut value = json!({
        "repo_root": env.config.repo_root.display().to_string(),
        "managed_root": env.config.managed_root.display().to_string(),
        "mock_worker": env.config.mock_worker.display().to_string(),
        "mock_worker_args": env.config.mock_worker_args,
        "worker_timeout_ms": env.config.worker_timeout_ms,
        "rules": [{"id": "mock_rule", "executable": env.config.rules[0].executable,
                    "argv": [], "timeout_ms": 5_000}],
    });
    if let Some(point) = stall {
        value["stall_at"] = json!(point);
    }
    if let Some(marker) = marker {
        value["stall_file"] = json!(marker.display().to_string());
    }
    value
}

fn write_config(env: &Env, stall: Option<&str>, marker: Option<&Path>) -> PathBuf {
    let path = env.dir.join("relay-config.json");
    fs::write(
        &path,
        serde_json::to_string(&config_json(env, stall, marker)).unwrap(),
    )
    .unwrap();
    path
}

fn spawn_daemon(env: &Env, stall: Option<&str>, marker: Option<&Path>) -> (Child, PathBuf) {
    let instance = env.dir.join("instance");
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = write_config(env, stall, marker);
    let exe = env!("CARGO_BIN_EXE_relayctl");
    let child = Command::new(exe)
        .arg("daemon")
        .arg("--instance-dir")
        .arg(&instance)
        .arg("--config")
        .arg(&config_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let socket = instance.join("relay.sock");
    (child, socket)
}

fn call(socket: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    use std::io::{BufRead, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let hello = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1}});
    writeln!(stream, "{hello}").unwrap();
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0","id":2,"method":method,"params":params});
    writeln!(stream, "{request}").unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn wait_state(socket: &Path, task_id: &str, want: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let reply = call(
            socket,
            "relay_status",
            serde_json::json!({"task_id": task_id}),
        );
        if reply["result"]["task"]["state"] == serde_json::json!(want) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

#[test]
fn foreground_term_reaps_active_worker_and_removes_owned_socket() {
    let env = setup("term-foreground", Some("hold"));
    let (mut child, socket) = spawn_daemon(&env, None, None);
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let plan = call(
        &socket,
        "relay_plan",
        json!({
            "project_id": "proj_term", "goal":"shutdown test", "brief":"local only",
            "write_scope":["src/count.rs"], "profile_id":"prof_mock",
            "verification_rule_ids":["mock_rule"], "base_oid":env.head,
        }),
    );
    let task = call(
        &socket,
        "relay_submit",
        json!({
            "plan_id":plan["result"]["plan_id"],
            "request_id":"7c9e6679-7425-40de-944b-e07fc1f90ae8",
        }),
    );
    let id = task["result"]["task"]["id"].as_str().unwrap();
    assert!(wait_state(&socket, id, "running", Duration::from_secs(8)));
    let instance = env.dir.join("instance");
    let store = Store::open(&instance.join("relay.sqlite3")).unwrap();
    let group = store
        .list_managed_groups(Some("tracking"))
        .unwrap()
        .into_iter()
        .find(|gate| gate.task_id == id)
        .unwrap();
    assert!(group.pgid > 0 && relay_sys::group_has_members(group.pgid).unwrap());

    assert!(Command::new("/bin/kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = relay_sys::killpg_kill(group.pgid);
            panic!("graceful stop exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "SIGTERM bypassed normal cleanup: {status}"
    );
    assert!(!socket.exists() && !instance.join("relay.sock.own").exists());
    assert!(!relay_sys::group_has_members(group.pgid).unwrap());
    assert!(store.list_pending_gates().unwrap().is_empty());
    let restarted = relay_daemon::start(&instance, "2026-11-22T02:00:00Z").unwrap();
    assert_eq!(
        restarted
            .store()
            .get_task(id)
            .unwrap()
            .unwrap()
            .state
            .to_string(),
        "interrupted"
    );
    assert!(restarted.store().get_artifact(id, 1).unwrap().is_none());
    drop(restarted);
    fs::remove_dir_all(&env.dir).unwrap();
}

/// T08 强杀矩阵（可重复注入，与真实断电测试分开记录）：在栅栏前、
/// 栅栏后未 spawn、spawn 后、发布与登记之间 kill -9 临时 daemon；
/// 同一实例按受管规则重启（陈旧 socket 接管），旧 attempt 不重复
/// 启动，未登记制品不可见，重试被阻断。
#[test]
fn kill9_at_stall_points_restart_within_managed_rules() {
    let points = [
        ("before_fence", "preparing", false),
        ("after_fence", "launching", false),
        ("after_spawn", "running", false),
        ("published_before_register", "verifying", true),
    ];
    for (point, want_state, artifact_dir_expected) in points {
        let short = match point {
            "before_fence" => "bf",
            "after_fence" => "af",
            "after_spawn" => "as",
            _ => "pr",
        };
        let env = setup(short, None);
        let marker = env.dir.join("stall.marker");
        fs::write(&marker, "hold").unwrap();
        let (mut child, socket) = spawn_daemon(&env, Some(point), Some(&marker));
        // 等 socket 就绪。
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // plan + submit。
        let plan = call(
            &socket,
            "relay_plan",
            serde_json::json!({
                "project_id": "proj_kill", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"], "base_oid": env.head,
            }),
        );
        let plan_id = plan["result"]["plan_id"].as_str().unwrap().to_owned();
        let task = call(
            &socket,
            "relay_submit",
            serde_json::json!({
                "plan_id": plan_id, "request_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            }),
        );
        let task_id = task["result"]["task"]["id"].as_str().unwrap().to_owned();
        // 等待进入注入点状态。
        assert!(
            wait_state(&socket, &task_id, want_state, Duration::from_secs(30)),
            "注入点 {point} 未到达 {want_state}"
        );
        // published_before_register：等制品目录落盘（发布已发生）。
        let artifact_dir = env
            .config
            .managed_root
            .join("artifacts")
            .join(format!("{task_id}-1"));
        if artifact_dir_expected {
            for _ in 0..100 {
                if artifact_dir.is_dir() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(artifact_dir.is_dir(), "发布应已落盘");
        }
        // 强杀临时 daemon（SIGKILL）。
        child.kill().unwrap();
        child.wait().unwrap();
        // 崩溃残留 socket：陈旧死 socket 文件仍在。
        assert!(socket.exists(), "强杀后 socket 文件残留（陈旧）");

        // 同一实例按受管规则重启：陈旧 socket 接管，恢复把活动状态归入
        // interrupted；不抢陌生 socket 的负例由 ipc_socket 覆盖。
        // 强杀后立即重启：锁释放与进程 teardown 完成存在亚毫秒级窗口，
        // 测试侧做有限重试（产品语义不变：锁争用仍拒绝）。
        let mut daemon = None;
        for _ in 0..40 {
            match relay_daemon::start_m1(
                &env.dir.join("instance"),
                "2026-11-22T02:00:00Z",
                relay_daemon::ipc::IpcConfig::default(),
                env.config.clone(),
            ) {
                Ok(d) => {
                    daemon = Some(d);
                    break;
                }
                Err(relay_daemon::DaemonError::LockContended { .. }) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("重启失败（{point}）：{e}"),
            }
        }
        let daemon = daemon.unwrap_or_else(|| panic!("重启（{point}）在期限内未获得锁"));
        let store = daemon.store_arc();
        let final_task = store.get_task(&task_id).unwrap().unwrap();
        assert_eq!(
            final_task.state.to_string(),
            "interrupted",
            "{point}：恢复应归入 interrupted"
        );
        // 旧 attempt 不重复启动：无第二次 claim；attempt 行只有 1 条。
        let events = store.list_events(0, 100).unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "claim").count(),
            1,
            "{point}：不得重复领取"
        );
        // 未登记制品不可见：无论哪一点，get_artifact 都为 None。
        assert!(
            store.get_artifact(&task_id, 1).unwrap().is_none(),
            "{point}：未登记制品不得通过 API 发现"
        );
        if artifact_dir_expected {
            assert!(
                artifact_dir.is_dir(),
                "{point}：崩溃残留目录按保留策略未自动清理"
            );
        }
        // 重试被阻断（interrupted 未证实退出）：不创建 attempt。
        let service = M1Service::new(store.clone(), env.config.clone());
        let retry = service.dispatch(
            "relay_retry",
            Some(&serde_json::json!({
                "task_id": task_id, "plan_id": plan_id,
                "expected_revision": final_task.revision,
                "request_id": "af2d5d31-6c0b-4a86-9a3c-1f2e3d4c5b6a",
            })),
        );
        assert!(
            retry.as_ref().unwrap().get("error").is_some(),
            "{point}：重试应被阻断"
        );
        drop(daemon);
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// TERM 顽固子进程（忽略 TERM + 同组孙进程残留）→ 升级 SIGKILL 后确认
/// 退出并 cancelled；未证实退出时保持 cancelling、不派新 attempt。
#[test]
fn stubborn_worker_escalates_to_kill_and_confirms_cancel() {
    let env = setup("stubborn", Some("hold-strict"));
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_st", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"], "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({
                "plan_id": plan_id, "request_id": "3d6e1f2a-7b8c-4d5e-9f0a-1b2c3d4e5f60",
            })),
        )
        .unwrap();
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();

    // worker 后台执行（hold-strict：忽略 TERM + 孙进程）。
    let stop_for_thread = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_for_thread2 = stop_for_thread.clone();
    let worker = service.clone();
    let handle = std::thread::spawn(move || {
        worker.run_one_claim(&stop_for_thread2);
    });
    // 等待进入 running。
    for _ in 0..200 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "running"
    );

    // 取消：TERM 被忽略 → 升级 KILL → 确认退出 → cancelled。
    let current_rev = store.get_task(&task_id).unwrap().unwrap().revision;
    let cancelled = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task_id, "expected_revision": current_rev,
                "request_id": "9d1b3c2a-4e5f-4a6b-8c7d-0e1f2a3b4c5d",
            })),
        )
        .unwrap();
    assert_eq!(
        cancelled["task"]["state"],
        json!("cancelled"),
        "{cancelled}"
    );
    handle.join().unwrap();
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "cancelled"
    );
    // 未派新 attempt。
    assert_eq!(store.get_task(&task_id).unwrap().unwrap().active_attempt, 1);
    fs::remove_dir_all(&env.dir).ok();
}

/// T11（部分）：备份恢复后制品映射完好——VACUUM INTO 备份可打开，
/// 已登记制品引用与摘要仍在。
#[test]
fn backup_restore_keeps_artifact_mapping() {
    let env = setup("backup", None);
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let (task_id, attempt_no, _revision) = chain(&env, &service, &store);
    let record = store.get_artifact(&task_id, attempt_no).unwrap().unwrap();

    // 一致性备份。
    let backup_path = env.dir.join("backup.db");
    let conn = rusqlite_conn(&env.dir.join("instance/relay.db"));
    conn.execute(
        "VACUUM INTO ?1",
        [backup_path.display().to_string().as_str()],
    )
    .unwrap();
    // 从备份打开：制品映射与摘要仍在。
    let restored = Store::open(&backup_path).unwrap();
    let restored_record = restored
        .get_artifact(&task_id, attempt_no)
        .unwrap()
        .unwrap();
    assert_eq!(restored_record.artifact_dir, record.artifact_dir);
    assert_eq!(restored_record.patch_sha256, record.patch_sha256);
    assert_eq!(restored_record.handoff_sha256, record.handoff_sha256);
    // 备份可驱动验收摘要门（服务用备份数据核对同一磁盘制品）。
    let service_on_backup = M1Service::new(Arc::new(restored), env.config.clone());
    let reply = service_on_backup
        .dispatch(
            "relay_artifact",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "handoff",
            })),
        )
        .unwrap();
    assert!(reply.get("error").is_none(), "{reply:?}");
    fs::remove_dir_all(&env.dir).ok();
}

/// T09（部分）：mock 子进程非零退出与空差异都分类为失败，不发布、
/// 不验收。
#[test]
fn abnormal_worker_exits_are_classified_failed() {
    // 非零退出。
    let env = setup("exit9", None);
    let mut config = env.config.clone();
    config.mock_worker = env.dir.join("exit9.sh");
    fs::write(&config.mock_worker, "#!/bin/sh\nexit 9\n").unwrap();
    fs::set_permissions(&config.mock_worker, fs::Permissions::from_mode(0o755)).unwrap();
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), config.clone());
    let task_id = submit_simple(&env, &service);
    let stop = std::sync::atomic::AtomicBool::new(false);
    service.run_one_claim(&stop);
    for _ in 0..100 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "failed"
    );
    assert!(
        store.get_artifact(&task_id, 1).unwrap().is_none(),
        "非零退出不发布"
    );
    fs::remove_dir_all(&env.dir).ok();

    // 空差异（worker 成功退出但未改任何文件）。
    let env = setup("emptydiff", None);
    let mut config = env.config.clone();
    config.mock_worker = env.dir.join("noop.sh");
    fs::write(&config.mock_worker, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&config.mock_worker, fs::Permissions::from_mode(0o755)).unwrap();
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), config.clone());
    let task_id = submit_simple(&env, &service);
    let stop = std::sync::atomic::AtomicBool::new(false);
    service.run_one_claim(&stop);
    for _ in 0..100 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "failed"
    );
    assert!(
        store.get_artifact(&task_id, 1).unwrap().is_none(),
        "空差异不发布"
    );
    fs::remove_dir_all(&env.dir).ok();
}

// ---- 共用小工具 ----

fn submit_simple(env: &Env, service: &M1Service) -> String {
    submit_simple_id(env, service, "c0ffee11-1d5c-4f7a-9e2b-3a4b5c6d7e8f")
}

fn submit_simple_id(env: &Env, service: &M1Service, request_id: &str) -> String {
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_t09", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"], "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({
                "plan_id": plan_id, "request_id": request_id,
            })),
        )
        .unwrap();
    task["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("submit 失败: {task:?}"))
        .to_owned()
}

fn chain(env: &Env, service: &M1Service, store: &Store) -> (String, u32, u64) {
    let task_id = submit_simple(env, service);
    let stop = std::sync::atomic::AtomicBool::new(false);
    service.run_one_claim(&stop);
    for _ in 0..300 {
        let task = store.get_task(&task_id).unwrap().unwrap();
        if task.state.to_string() == "awaiting_review" {
            return (task_id, task.active_attempt, task.revision);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("未进入 awaiting_review");
}

fn rusqlite_conn(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

/// 统一子进程监管（05/08/A3）：组长退出 ≠ 整组退出。父进程已退出、同组
/// 后代仍存活且忽略 TERM → 有界回收（TERM → SIGKILL）确认整组退出后任务
/// 失败、不产生可验收候选；后代不得残留，managed group 收口为 resolved。
#[test]
fn leader_exit_with_live_descendant_is_reclaimed_before_failure() {
    let env = setup("orphan-group", Some("orphan"));
    let mut config = env.config.clone();
    config.reclaim_term_ms = 400;
    config.reclaim_kill_ms = 400;
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), config);
    let task_a = submit_simple_id(&env, &service, "0a0b0000-0000-4000-8000-0000000000aa");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runner = {
        let service = service.clone();
        let stop = stop.clone();
        std::thread::spawn(move || service.run_one_claim(&stop))
    };
    // Capture the worker's process group before reclaim settles it.
    let mut pgid = None;
    for _ in 0..200 {
        if let Some(group) = store
            .list_managed_groups(Some("tracking"))
            .unwrap()
            .into_iter()
            .find(|g| g.task_id == task_a)
        {
            pgid = Some(group.pgid);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pgid = pgid.expect("worker group should be tracked before exit");
    runner.join().unwrap();
    for _ in 0..200 {
        let state = store.get_task(&task_a).unwrap().unwrap().state.to_string();
        if state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // 回收确认后任务失败（不进入待验收），不产生可验收候选。
    assert_eq!(
        store.get_task(&task_a).unwrap().unwrap().state.to_string(),
        "failed"
    );
    assert!(store.get_artifact(&task_a, 1).unwrap().is_none());
    // 后代必须已被有界回收：不依赖其 300 秒自限退出。
    assert!(
        !relay_sys::group_has_members(pgid).unwrap(),
        "组长退出后同组后代必须被回收"
    );
    // managed group 已 resolved；gate 不残留 pending。
    assert!(
        store
            .list_managed_groups(Some("tracking"))
            .unwrap()
            .iter()
            .all(|g| g.task_id != task_a),
        "managed group 应已 resolved"
    );
    assert!(
        !store
            .list_pending_gates()
            .unwrap()
            .iter()
            .any(|g| g.task_id == task_a),
        "回收确认后不应残留 pending gate"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// review/cancel 并发（确定性同步点）：同一 revision 只有一个操作能
/// 提交——另一个收到 STALE_REVISION/WRONG_STATE，事件恰一条终态迁移。
#[test]
fn concurrent_review_and_cancel_commit_only_one() {
    let env = setup("race", None);
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let (task_id, attempt_no, revision) = chain(&env, &service, &store);
    let record = store.get_artifact(&task_id, attempt_no).unwrap().unwrap();

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let svc_a = service.clone();
    let svc_b = service.clone();
    let b1 = barrier.clone();
    let b2 = barrier.clone();
    let task_a = task_id.clone();
    let task_b = task_id.clone();
    let review = std::thread::spawn(move || {
        b1.wait();
        svc_a.dispatch(
            "relay_review",
            Some(&serde_json::json!({
                "task_id": task_a, "attempt_no": attempt_no,
                "expected_revision": revision,
                "request_id": "aaaa1111-2222-4333-8444-555566667777",
                "verdict": "accept", "diff_sha256": record.patch_sha256,
                "test_sha256": record.receipt_sha256,
            })),
        )
    });
    let cancel = std::thread::spawn(move || {
        b2.wait();
        svc_b.dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task_b, "expected_revision": revision,
                "request_id": "cccc3333-4444-4555-8666-777788889999",
            })),
        )
    });
    let review_reply = review.join().unwrap().unwrap();
    let cancel_reply = cancel.join().unwrap().unwrap();
    // 恰一个成功。
    let review_ok = review_reply.get("error").is_none();
    let cancel_ok = cancel_reply.get("error").is_none();
    assert!(
        review_ok ^ cancel_ok,
        "并发下恰一个操作提交：review={review_reply:?} cancel={cancel_reply:?}"
    );
    // 败者收到明确的 stale/wrong-state 错误。
    let loser = if review_ok {
        &cancel_reply
    } else {
        &review_reply
    };
    let text = format!("{loser:?}");
    assert!(
        text.contains("STALE_REVISION") || text.contains("WRONG_STATE"),
        "{text}"
    );
    // 终态一致且事件恰一条终态迁移。
    let final_state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
    assert!(
        final_state == "completed" || final_state == "cancelling" || final_state == "cancelled",
        "{final_state}"
    );
    let events = store.list_events(0, 100).unwrap();
    let terminal = events
        .iter()
        .filter(|e| e.kind == "review_accepted" || e.kind == "cancel_requested")
        .count();
    assert_eq!(terminal, 1, "只允许一个操作产生终态迁移事件: {events:?}");
    // 若进入 cancelling（cancel 赢且无 owned 进程组残留路径），由取消
    // 发起方确认回收；本测试 worker 已完成退出，无受管进程。
    fs::remove_dir_all(&env.dir).ok();
}

// ---- 进程控制替身（注入信号失败 / 探测错误 / 永不退出） ----

use relay_daemon::service::GroupControl;

/// 信号全部失败、探测按替身设定返回。
struct FaultControl {
    probe_result: fn() -> std::io::Result<bool>,
}

impl GroupControl for FaultControl {
    fn term(&self, _pgid: i32) -> std::io::Result<()> {
        Err(std::io::Error::other("注入：TERM 信号失败"))
    }
    fn kill(&self, _pgid: i32) -> std::io::Result<()> {
        Err(std::io::Error::other("注入：KILL 信号失败"))
    }
    fn probe(&self, _pgid: i32) -> std::io::Result<bool> {
        (self.probe_result)()
    }
}

fn env_with_control(
    name: &str,
    worker_arg: Option<&str>,
    control: Arc<dyn GroupControl>,
) -> (Env, Arc<M1Service>, Arc<Store>) {
    let mut env = setup(name, worker_arg);
    env.config.reclaim_term_ms = 400;
    env.config.reclaim_kill_ms = 400;
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::with_group_control(store.clone(), env.config.clone(), control);
    (env, service, store)
}

fn spawn_and_wait_running(
    env: &Env,
    service: &std::sync::Arc<M1Service>,
    store: &Store,
    _request_id: &str,
) -> String {
    let task_id = submit_simple(env, service);
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop3 = stop2.clone();
    let worker = service.clone();
    let _handle = std::thread::spawn(move || worker.run_one_claim(&stop3));
    for _ in 0..200 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "running" {
            return task_id;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    panic!("未进入 running");
}

/// 取消 + 探测始终报存活（子进程始终不退出的替身）：期限到返回
/// Unconfirmed——状态保持 cancelling（不误判 cancelled）、不派新
/// attempt、取消调用按期限返回。
#[test]
fn cancel_with_probe_alive_stays_cancelling_by_deadline() {
    let (env, service, store) = env_with_control(
        "probe-alive",
        Some("hold"),
        Arc::new(FaultControl {
            probe_result: || Ok(true),
        }),
    );
    // 缩短 worker 等待上限：worker 超时路径也会转 interrupted。
    let task_id = spawn_and_wait_running(
        &env,
        &service,
        &store,
        "11111111-2222-4333-8444-5555666677aa",
    );
    let current_rev = store.get_task(&task_id).unwrap().unwrap().revision;
    let started = std::time::Instant::now();
    let reply = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task_id, "expected_revision": current_rev,
                "request_id": "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e",
            })),
        )
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(6),
        "取消须按期限返回: {elapsed:?}"
    );
    assert_eq!(
        reply["task"]["state"],
        json!("cancelling"),
        "探测存活不得误判 cancelled: {reply:?}"
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "cancelling"
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().active_attempt,
        1,
        "不派新 attempt"
    );
    let events = store.list_events(0, 100).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "all_owned_workers_stopped"),
        "{events:?}"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// 执行超时 + 信号失败（TERM/KILL 均 Err、探测报存活）：期限到转
/// interrupted 阻塞——不进可重试失败态、无终态误判、无新 attempt。
#[test]
fn timeout_with_signal_failure_marks_interrupted() {
    let (env, _service, store) = env_with_control(
        "sig-fail",
        Some("hold"),
        Arc::new(FaultControl {
            probe_result: || Ok(true),
        }),
    );
    let mut config = env.config.clone();
    config.worker_timeout_ms = 700;
    let service = M1Service::with_group_control(
        store.clone(),
        config.clone(),
        Arc::new(FaultControl {
            probe_result: || Ok(true),
        }),
    );
    let task_id = submit_simple(&env, &service);
    let (tx, rx) = std::sync::mpsc::channel();
    let svc = service.clone();
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop3 = stop2.clone();
    std::thread::spawn(move || {
        svc.run_one_claim(&stop3);
        let _ = tx.send(());
    });
    // 等待进入 running（hold worker），随后 worker 等待循环 700ms 超时。
    for _ in 0..100 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    // worker 线程按期限结束（回收失败注入下不无期限阻塞）。
    let joined = rx.recv_timeout(Duration::from_secs(8));
    assert!(
        joined.is_ok(),
        "worker 须按期限返回（不得无期限 child.wait）"
    );
    let state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
    assert_eq!(state, "interrupted", "超时+信号失败应转 interrupted 阻塞");
    // 无终态误判：非 failed/cancelled；无新 attempt。
    assert_ne!(state, "failed");
    assert_ne!(state, "cancelled");
    assert_eq!(store.get_task(&task_id).unwrap().unwrap().active_attempt, 1);
    fs::remove_dir_all(&env.dir).ok();
}

/// 探测错误（权限/IO）按不可确认处理：取消保持 cancelling，超时转
/// interrupted——探测错误不产生已退出证据。
#[test]
fn probe_error_is_never_confirmed() {
    let (env, service, store) = env_with_control(
        "probe-err",
        Some("hold"),
        Arc::new(FaultControl {
            probe_result: || Err(std::io::Error::other("注入：探测权限错误")),
        }),
    );
    let task_id = spawn_and_wait_running(
        &env,
        &service,
        &store,
        "44445555-6666-4777-8888-9999aaaabbbb",
    );
    let current_rev = store.get_task(&task_id).unwrap().unwrap().revision;
    let started = std::time::Instant::now();
    let reply = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task_id, "expected_revision": current_rev,
                "request_id": "5c6d7e8f-9a0b-4c1d-2e3f-4a5b6c7d8e9f",
            })),
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(6), "按期限返回");
    assert_eq!(
        reply["task"]["state"],
        json!("cancelling"),
        "探测错误不得误判 cancelled: {reply:?}"
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "cancelling"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// 探测谎报"组已消失"而真实子进程仍存活：收割必须有界返回（旧实现
/// 在此处无期限 child.wait 阻塞）。
#[test]
fn confirmed_with_live_child_reaps_by_deadline() {
    let (env, _service, store) = env_with_control(
        "live-child",
        Some("hold"),
        Arc::new(FaultControl {
            probe_result: || Ok(false),
        }),
    );
    let mut config = env.config.clone();
    config.worker_timeout_ms = 700;
    let service = M1Service::with_group_control(
        store.clone(),
        config.clone(),
        Arc::new(FaultControl {
            probe_result: || Ok(false),
        }),
    );
    let task_id = submit_simple(&env, &service);
    let (tx, rx) = std::sync::mpsc::channel();
    let svc = service.clone();
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || {
        svc.run_one_claim(&stop2);
        let _ = tx.send(());
    });
    // 等待进入 running（hold worker），等待循环 700ms 超时 → 回收
    // （探测谎报 confirmed）→ 有界收割：旧实现在此处无期限阻塞。
    for _ in 0..100 {
        if store.get_task(&task_id).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let joined = rx.recv_timeout(Duration::from_secs(8));
    assert!(joined.is_ok(), "收割须有界返回（不得无期限 child.wait）");
    fs::remove_dir_all(&env.dir).ok();
}

/// M2 离线样板：一条命令跑全部用例（模拟输入）；候选路线默认禁用。
#[test]
fn adapter_sample_cases_all_pass_and_route_disabled() {
    let cases = relay_daemon::adapter_sample::run_sample_cases();
    assert!(cases.len() >= 10, "用例矩阵应完整: {}", cases.len());
    for (name, verdict, detail) in &cases {
        let pass = detail.starts_with("PASS");
        assert!(pass, "用例 {name} 应通过: {verdict:?} {detail}");
        let _ = verdict;
    }
    let report = relay_daemon::adapter_sample::cases_json(&cases);
    assert_eq!(report["all_pass"], json!(true));
    let route = &report["routes"][0];
    assert_eq!(route["id"], json!("claude_code_minimax"));
    assert_eq!(route["enabled"], json!(false), "候选路线默认禁用");
    assert!(route["requirements"].as_array().unwrap().len() >= 4);
}

/// 环境变量锁：ADAPTER_* 是进程级变量，改它的测试必须串行，否则同一
/// 测试二进制内并行用例会互相污染夹具行为。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 探测模式可切换的替身：1 = 组存活，0 = 组已退出（可信退出证据）。
/// 信号真实下发（进程确实被回收，不泄漏），只有探测结果受控。
struct SwitchProbeControl {
    mode: Arc<std::sync::atomic::AtomicUsize>,
}

impl GroupControl for SwitchProbeControl {
    fn term(&self, pgid: i32) -> std::io::Result<()> {
        relay_sys::killpg_term(pgid)
    }
    fn kill(&self, pgid: i32) -> std::io::Result<()> {
        relay_sys::killpg_kill(pgid)
    }
    fn probe(&self, _pgid: i32) -> std::io::Result<bool> {
        use std::sync::atomic::Ordering;
        Ok(self.mode.load(Ordering::SeqCst) == 1)
    }
}

/// 已独立复现回归（Codex@5588049）：任务 A 超时且回收未证实（TERM/KILL
/// 失败、探测仍存活）→ A 进入 interrupted；提交不同 request_id 的任务 B
/// 后一次 run_one_claim 曾错误领取并启动 B。持久派发门修复后：B 保持
/// queued、无 claim/栅栏/启动记录；探测证实组退出后恢复领取，B 恰好
/// 启动一次，A 不重跑。同一进程与重启（Store 重开）各验一次。
#[test]
fn dispatch_gate_blocks_b_until_group_exit_confirmed() {
    let mode = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let env = setup("gate-ab", Some("hold"));
    let mut config = env.config.clone();
    config.worker_timeout_ms = 600;
    config.reclaim_term_ms = 300;
    config.reclaim_kill_ms = 300;
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::with_group_control(
        store.clone(),
        config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );

    // 任务 A：提交 → 领取 → 运行 → 超时 → 回收未证实 → interrupted + 阻断。
    let task_a = submit_simple_id(&env, &service, "aaaa0000-0000-4000-8000-00000000000a");
    let (tx, rx) = std::sync::mpsc::channel();
    let svc = service.clone();
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || {
        svc.run_one_claim(&stop2);
        let _ = tx.send(());
    });
    for _ in 0..100 {
        if store.get_task(&task_a).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    rx.recv_timeout(Duration::from_secs(8))
        .expect("A 超时路径须按期限返回");
    // A：回收未证实（探测恒报存活）→ 不进入待验收。
    let a_state = store.get_task(&task_a).unwrap().unwrap().state.to_string();
    assert!(
        a_state == "interrupted" || a_state == "failed",
        "回收未确认时 A 不应进入待验收：{a_state}"
    );
    // A 的待确认标记仍 pending（持久阻断）。
    let a_gate = store
        .list_pending_gates()
        .unwrap()
        .into_iter()
        .find(|g| g.task_id == task_a)
        .expect("A 应有 pending 待确认标记");
    assert_eq!(a_gate.state, "pending");

    // 任务 B：不同 request_id 提交。
    let task_b = submit_simple_id(&env, &service, "bbbb0000-0000-4000-8000-00000000000b");
    let claims_before = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim")
        .count();

    // 一次领取尝试：组仍存活 → 门阻断，B 保持 queued。
    let stop = std::sync::atomic::AtomicBool::new(false);
    let claimed = service.run_one_claim(&stop);
    assert!(!claimed, "派发门阻断下不得领取 B");
    assert_eq!(
        store.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "queued"
    );
    let claims_after = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim")
        .count();
    assert_eq!(claims_after, claims_before, "不得新增 claim");
    // B 无栅栏记录。
    assert!(
        store.get_launch_fence(&task_b, 1).unwrap().is_none(),
        "B 不得有栅栏"
    );

    // —— 同一进程：确认退出（探测 0）→ 解除 → 恢复领取，B 恰好一次。
    mode.store(0, std::sync::atomic::Ordering::SeqCst);
    // 领取在后台线程执行（run_one_claim 同步跑完整个 attempt）。
    let stop_b = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_b2 = stop_b.clone();
    let svc_b = service.clone();
    let handle_b = std::thread::spawn(move || svc_b.run_one_claim(&stop_b2));
    let mut b_running = false;
    for _ in 0..200 {
        if store.get_task(&task_b).unwrap().unwrap().state.to_string() == "running" {
            b_running = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(b_running, "B 应进入 running（已领取并启动）");
    let _ = handle_b.join();
    let b_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_b)
        .count();
    assert_eq!(b_claims, 1, "B 恰好启动一次");
    let a_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_a)
        .count();
    assert_eq!(a_claims, 1, "A 不重跑");
    fs::remove_dir_all(&env.dir).ok();
}

/// 重启变体：Store 重开（模拟重启）后阻断仍生效，解除后 B 恰好一次。
#[test]
fn dispatch_gate_survives_restart_then_resumes() {
    let mode = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let env = setup("gate-restart", Some("hold"));
    let mut config = env.config.clone();
    config.worker_timeout_ms = 600;
    config.reclaim_term_ms = 300;
    config.reclaim_kill_ms = 300;
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::with_group_control(
        store.clone(),
        config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );
    let task_a = submit_simple_id(&env, &service, "aaaa0000-0000-4000-8000-00000000000a");
    let (tx, rx) = std::sync::mpsc::channel();
    let svc = service.clone();
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn(move || {
        svc.run_one_claim(&stop2);
        let _ = tx.send(());
    });
    for _ in 0..100 {
        if store.get_task(&task_a).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    rx.recv_timeout(Duration::from_secs(8))
        .expect("A 超时路径按期限返回");

    // "重启"：Store 重开 + 新 service（内存 guard 全新）。
    let store2: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service2 = M1Service::with_group_control(
        store2.clone(),
        config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );
    let task_b = submit_simple_id(&env, &service2, "bbbb0000-0000-4000-8000-00000000000b");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let claimed = service2.run_one_claim(&stop);
    assert!(!claimed, "重启后阻断仍生效（持久记录）");
    assert_eq!(
        store2.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "queued"
    );

    // 确认退出 → 恢复领取 → B 恰好一次。
    mode.store(0, std::sync::atomic::Ordering::SeqCst);
    let claimed = service2.run_one_claim(&stop);
    assert!(claimed, "解除后应恢复领取");
    for _ in 0..100 {
        if store2.get_task(&task_b).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let b_claims = store2
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_b)
        .count();
    assert_eq!(b_claims, 1, "B 恰好启动一次");
    fs::remove_dir_all(&env.dir).ok();
}

/// M2 适配链路（05 事件收敛面）：fixture 子进程经 daemon 业务跑通
/// success / blocked（身份与能力）/ failed（auth/rate/protocol）路径；
/// 验证失败不产生可验收候选由 m1_e2e::verification_failure 覆盖。
#[test]
fn adapter_chain_cases_cover_stop_paths() {
    let cases = relay_daemon::adapter_sample::run_sample_cases();
    assert!(cases.len() >= 10);
    // 链路用例经 relayctl adapter-check 驱动（真实子进程 + daemon 业务），
    // 退出码在批末验收记录；此处断言判定函数的停止路径分类。
    let (v, d) = relay_daemon::adapter_sample::check_stop_conditions(
        &relay_daemon::adapter_sample::SimFrame::AuthFailed {
            provider: "minimax".into(),
        },
    );
    assert!(d.contains("不切换账号"), "{d}");
    assert!(matches!(v, relay_daemon::adapter_sample::Verdict::Stop));
    let (v, d) = relay_daemon::adapter_sample::check_stop_conditions(
        &relay_daemon::adapter_sample::SimFrame::RateLimited {
            reason: "quota".into(),
        },
    );
    assert!(d.contains("不自动转备用路线"), "{d}");
    assert!(matches!(v, relay_daemon::adapter_sample::Verdict::Stop));
    let (v, _) =
        relay_daemon::adapter_sample::check_model_identity("fake-adapter-v1", Some("other"));
    assert!(matches!(v, relay_daemon::adapter_sample::Verdict::Reject));
}

/// 已复现回归（Codex@c951a9b）：临时库中 managed_groups INSERT 失败，
/// 且回收探测无法确认退出——A 进入 interrupted 后领取 B 时旧版会领取
/// B。修复后：fence 事务失败即不启动（A 无 spawn），B 保持 queued、无
/// claim/栅栏/启动记录。
#[test]
fn fence_transaction_failure_prevents_spawn_and_blocks_next_claim() {
    let env = setup("fence-fail", Some("hold"));
    let db = env.dir.join("instance/relay.db");
    let store: Arc<Store> = Arc::new(Store::open(&db).unwrap());
    // 回收探测无法确认退出（替身恒报存活）→ gate 不自动解封。
    let mode = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let service = M1Service::with_group_control(
        store.clone(),
        env.config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );

    // 注入：managed_groups 的 INSERT 失败（模拟临时库写失败）。
    let conn = store.conn().unwrap();
    conn.execute_batch(
        "CREATE TRIGGER wp_fail_group_insert BEFORE INSERT ON managed_groups
         BEGIN SELECT RAISE(ABORT, 'injected managed_groups insert failure'); END;",
    )
    .unwrap();
    drop(conn);

    // 任务 A：领取 → fence 事务失败 → 不 spawn（无 launched/running）。
    let task_a = submit_simple_id(&env, &service, "f00d0000-0000-4000-8000-00000000000a");
    let stop = std::sync::atomic::AtomicBool::new(false);
    service.run_one_claim(&stop);
    // A：启动许可已提交（gate 标记 pending），spawn 后组登记失败 →
    // 缺进程记录、退出未确认 → interrupted，gate 保持阻断。
    let a_state = store.get_task(&task_a).unwrap().unwrap().state.to_string();
    assert_eq!(a_state, "interrupted", "组登记失败后 A 应 interrupted");
    let a_events = store.list_events(0, 100).unwrap();
    assert!(
        !a_events.iter().any(|e| e.kind == "spawned"),
        "组登记失败不得确认 spawn: {a_events:?}"
    );
    // gate 标记 pending（durable，待确认）。
    let gates = store.list_pending_gates().unwrap();
    assert!(
        gates.iter().any(|g| g.task_id == task_a),
        "gate 标记应 pending: {gates:?}"
    );

    // 任务 B：不同 request_id；领取必须被阻断。
    let task_b = submit_simple_id(&env, &service, "f00d0000-0000-4000-8000-00000000000b");
    let claimed = service.run_one_claim(&stop);
    assert!(!claimed, "写入失败后不得领取 B");
    assert_eq!(
        store.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "queued"
    );
    let b_events = store.list_events(0, 100).unwrap();
    assert!(
        !b_events
            .iter()
            .any(|e| e.kind == "claim" && e.task_id == task_b),
        "B 不得有 claim: {b_events:?}"
    );
    assert!(
        store.get_launch_fence(&task_b, 1).unwrap().is_none(),
        "B 不得有栅栏"
    );

    // 可信证据（组退出已证实）→ 解封 → B 恰好启动一次，A 不重跑。
    mode.store(0, std::sync::atomic::Ordering::SeqCst);
    // A 的组记录缺失（登记失败）→ 无 pgid 记录，不能自动解封：需人工
    // 确认。这里用"从未启动"的可信证据路径：把 A 的 gate 释放（模拟
    // 运维确认进程已不存在）。
    store
        .release_dispatch_gate(&task_a, 1, "人工确认进程不存在", "2026-11-23T00:00:00Z")
        .unwrap();
    let stop2 = std::sync::atomic::AtomicBool::new(false);
    let claimed = service.run_one_claim(&stop2);
    assert!(claimed, "解封后应恢复领取");
    for _ in 0..100 {
        if store.get_task(&task_b).unwrap().unwrap().state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let b_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_b)
        .count();
    assert_eq!(b_claims, 1, "B 恰好启动一次");
    let a_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_a)
        .count();
    assert_eq!(a_claims, 1, "A 不重跑");
    fs::remove_dir_all(&env.dir).ok();
}

/// 三窗口覆盖（同一组 A/B 夹具，确定性同步点）：启动许可已提交但未
/// spawn、已 spawn 但未登记进程组、登记/写回失败且退出不明——三窗口
/// 重启后都仍阻断；不自动解封（缺进程记录/探测不出）；只有可信证据
/// 才解封；解封后 A 不重跑、B 恰好启动一次。
#[test]
fn three_windows_stay_blocked_after_restart_until_trusted_evidence() {
    for (window, stall_point) in [
        ("w1", Some("after_fence")),
        ("w2", Some("after_spawn_before_register")),
    ] {
        let env = setup(window, Some("hold"));
        let mut config = env.config.clone();
        config.stall_at = stall_point.map(str::to_owned);
        config.stall_file = Some(env.dir.join("stall.marker"));
        fs::write(env.dir.join("stall.marker"), "hold").unwrap();
        let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
        let service = M1Service::new(store.clone(), config.clone());
        let task_a = submit_simple_id(&env, &service, "22220000-0000-4000-8000-00000000a001");
        // worker 走到同步点后停住（许可已提交）。
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let svc = service.clone();
        let handle = std::thread::spawn(move || svc.run_one_claim(&stop2));
        for _ in 0..200 {
            if store
                .list_pending_gates()
                .unwrap()
                .iter()
                .any(|g| g.task_id == task_a)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        // 窗口 1：许可已提交、尚未 spawn（pgid 未知）——无可信证据。
        // 窗口 2：已 spawn、组记录尚未写入——缺进程记录。
        let gates = store.list_pending_gates().unwrap();
        assert!(
            gates.iter().any(|g| g.task_id == task_a),
            "{window}：应有 pending gate 标记: {gates:?}"
        );

        // "重启"：Store 重开 + 新 service（内存状态全新）。
        let store2: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
        let service2 = M1Service::new(store2.clone(), config.clone());
        let task_b = submit_simple_id(&env, &service2, "33330000-0000-4000-8000-00000000b002");
        let stop3 = std::sync::atomic::AtomicBool::new(false);
        let claimed = service2.run_one_claim(&stop3);
        assert!(!claimed, "{window}：重启后仍须阻断（B 保持 queued）");
        assert_eq!(
            store2.get_task(&task_b).unwrap().unwrap().state.to_string(),
            "queued"
        );
        let b_events = store2.list_events(0, 100).unwrap();
        assert!(
            !b_events
                .iter()
                .any(|e| e.kind == "claim" && e.task_id == task_b),
            "{window}：B 不得有 claim"
        );
        assert!(
            store2.get_launch_fence(&task_b, 1).unwrap().is_none(),
            "{window}：B 不得有栅栏"
        );

        // 释放同步点并结束 worker（可信证据：进程已退出）。
        let _ = fs::remove_file(env.dir.join("stall.marker"));
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = handle.join();

        // 人工确认（可信证据）后解封 → B 恰好启动一次，A 不重跑。
        store2
            .release_dispatch_gate(&task_a, 1, "人工确认进程不存在", "2026-11-23T00:00:00Z")
            .unwrap();
        let claimed = service2.run_one_claim(&stop3);
        assert!(claimed, "{window}：解封后应恢复领取");
        for _ in 0..100 {
            if store2.get_task(&task_b).unwrap().unwrap().state.to_string() == "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let b_claims = store2
            .list_events(0, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "claim" && e.task_id == task_b)
            .count();
        assert_eq!(b_claims, 1, "{window}：B 恰好启动一次");
        let a_claims = store2
            .list_events(0, 100)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "claim" && e.task_id == task_a)
            .count();
        assert_eq!(a_claims, 1, "{window}：A 不重跑");
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// M2 适配器协议负例（定向）：probe 挂起超限、单帧超限、stdout 被后代
/// 持有、重复/冲突终止帧——均在期限内结束且不产生可验收候选；后一帧
/// 不覆盖已确定的终止结果。
#[test]
fn adapter_protocol_negative_cases_end_within_deadline() {
    use relay_daemon::adapter_sample::{run_sample_cases, Verdict};
    // 判定层：重复终止帧计数、冲突终止帧计数、帧上限与读取上限参数。
    let cases = run_sample_cases();
    assert!(cases.len() >= 10);
    assert!(cases.iter().any(|(n, _, _)| *n == "diagnostics_redacted"));

    // 链路层：超限/冲突/后代持管道 → failed（不进入 awaiting_review）。
    // 由 relayctl adapter-check 的链路用例驱动（真实子进程）。
    let exe = env!("CARGO_BIN_EXE_relayctl");
    let out = Command::new(exe).arg("adapter-check").output().unwrap();
    assert!(out.status.success(), "adapter-check 应通过: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find('{').expect("JSON 输出");
    let end = text.rfind('}').expect("JSON 结束");
    let report: serde_json::Value = serde_json::from_str(&text[start..=end]).unwrap();
    let by_name =
        |name: &str| -> bool {
            report["chain_cases"].as_array().unwrap().iter().any(|c| {
                c["name"].as_str().unwrap_or("").starts_with(name) && c["pass"] == json!(true)
            })
        };
    for case in [
        "huge_frame",
        "conflicting_terminal",
        "hold_stdout",
        "alive_no_pipe",
        "hang",
        "auth_failed",
        "rate_limited",
        "unsupported_protocol",
        "probe_hang",
    ] {
        assert!(by_name(case), "负例 {case} 应在期限内失败且不产候选");
    }
    // 原成功链路保留。
    assert!(by_name("success/fake-adapter-v1"), "成功链路应保留");
    // 判定层 Verdict 仍可用（未被移除）。
    assert!(matches!(
        relay_daemon::adapter_sample::check_model_identity("x", Some("x")).0,
        Verdict::Pass
    ));
}

/// 已复现回归（Codex@d82b5bc）：组长退出、后代仍持 stdout，读取超时后
/// 适配器读超时 + 后代持有 stdout 管道（A3）：组长退出 ≠ 整组退出。
/// 有界回收确认整组退出后任务失败、gate 不残留 pending；随后 B 可领取
/// 一次，A 不重跑。
#[test]
fn leader_exit_with_pipe_holder_is_reclaimed_and_unblocks_claims() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 适配器夹具：父进程退出、后代持有 stdout 管道。
    let env = setup("pipe-holder", None);
    let mut config = env.config.clone();
    config.adapter_fixture =
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/mock_adapter.sh"));
    config.expected_model = Some("fake-adapter-v1".to_owned());
    config.adapter_read_timeout_ms = 700;
    config.adapter_probe_timeout_ms = 2_000;
    config.group_exit_budget_ms = 500;
    config.reclaim_term_ms = 400;
    config.reclaim_kill_ms = 400;
    config.worker_timeout_ms = 5_000;
    let store: Arc<Store> = Arc::new(Store::open(&env.dir.join("instance/relay.sqlite3")).unwrap());
    let service = M1Service::new(store.clone(), config.clone());
    // 让夹具进入 hold_stdout 模式（父退出、后代持管道）。
    std::env::set_var("ADAPTER_EMIT", "hold_stdout");
    std::env::set_var("ADAPTER_HOLD_SECONDS", "30");
    let task_a = submit_simple_id(&env, &service, "0c0d0000-0000-4000-8000-0000000000aa");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runner = {
        let service = service.clone();
        let stop = stop.clone();
        std::thread::spawn(move || service.run_one_claim(&stop))
    };
    let mut pgid = None;
    for _ in 0..200 {
        if let Some(group) = store
            .list_managed_groups(Some("tracking"))
            .unwrap()
            .into_iter()
            .find(|g| g.task_id == task_a)
        {
            pgid = Some(group.pgid);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let pgid = pgid.expect("adapter group should be tracked before reclaim");
    runner.join().unwrap();
    for _ in 0..200 {
        let state = store.get_task(&task_a).unwrap().unwrap().state.to_string();
        if state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        store.get_task(&task_a).unwrap().unwrap().state.to_string(),
        "failed",
        "读超时后任务失败"
    );
    // 后代已被有界回收：不依赖其 30 秒自限退出。
    assert!(
        !relay_sys::group_has_members(pgid).unwrap(),
        "持管道后代必须被回收"
    );
    // gate 不残留 pending；managed group 已 resolved。
    assert!(
        !store
            .list_pending_gates()
            .unwrap()
            .iter()
            .any(|g| g.task_id == task_a),
        "回收确认后不应残留 pending gate"
    );
    assert!(
        store
            .list_managed_groups(Some("tracking"))
            .unwrap()
            .iter()
            .all(|g| g.task_id != task_a),
        "managed group 应已 resolved"
    );

    // 回收后 B 可正常领取一次，A 不重跑。
    let task_b = submit_simple_id(&env, &service, "0e0f0000-0000-4000-8000-0000000000bb");
    let stop2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(service.run_one_claim(&stop2), "回收确认后应恢复领取");
    let b_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_b)
        .count();
    assert_eq!(b_claims, 1, "B 恰好启动一次");
    let a_claims = store
        .list_events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "claim" && e.task_id == task_a)
        .count();
    assert_eq!(a_claims, 1, "A 不重跑");
    std::env::remove_var("ADAPTER_EMIT");
    std::env::remove_var("ADAPTER_HOLD_SECONDS");
    fs::remove_dir_all(&env.dir).ok();
}

/// 受管恢复入口（正式 CLI）：缺进程记录 + 无运维确认 → 保持阻断；带
/// 运维确认与证据说明 → 解封并写审计（attempt、证据、结果、操作者）。
#[test]
fn managed_gate_recovery_entry_records_audit() {
    let env = setup("gate-audit", None);
    let db = env.dir.join("instance/relay.sqlite3");
    let store: Arc<Store> = Arc::new(Store::open(&db).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    // 造一个 pending gate（缺进程记录场景）：经真实 fence 路径。
    let task_id = submit_simple_id(&env, &service, "1a2b0000-0000-4000-8000-0000000000cc");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let _ = service.run_one_claim(&stop);
    // 手工置 pending gate（模拟 spawn 前中断），并移除组记录
    // （缺进程记录场景：无可信退出证据，只能运维确认）。
    let conn = store.conn().unwrap();
    conn.execute(
        "DELETE FROM managed_groups WHERE task_id = ?1",
        rusqlite_params(&task_id),
    )
    .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO dispatch_gate (task_id, attempt_no, state, reason, created_at, updated_at)
         VALUES (?1, 1, 'pending', '测试夹具：spawn 未确认', '2026-11-23T00:00:00Z', '2026-11-23T00:00:00Z')",
        rusqlite_params(&task_id),
    )
    .unwrap();
    drop(conn);

    let exe = env!("CARGO_BIN_EXE_relayctl");
    // 无证据 → 拒绝（保持阻断）。
    let no_evidence = Command::new(exe)
        .arg("gate")
        .arg("confirm")
        .arg("--instance-dir")
        .arg(env.dir.join("instance"))
        .arg("--task-id")
        .arg(&task_id)
        .output()
        .unwrap();
    assert!(!no_evidence.status.success(), "无证据不得解封");
    assert!(
        store
            .list_pending_gates()
            .unwrap()
            .iter()
            .any(|g| g.task_id == task_id),
        "拒绝后仍 pending"
    );

    // 运维确认 + 证据 → 解封并写审计。
    let confirmed = Command::new(exe)
        .arg("gate")
        .arg("confirm")
        .arg("--instance-dir")
        .arg(env.dir.join("instance"))
        .arg("--task-id")
        .arg(&task_id)
        .arg("--note")
        .arg("已核对进程表，确认无残留")
        .arg("--by")
        .arg("operator-a")
        .arg("--confirm-absent")
        .output()
        .unwrap();
    assert!(
        confirmed.status.success(),
        "运维确认应解封: stdout={} stderr={}",
        String::from_utf8_lossy(&confirmed.stdout),
        String::from_utf8_lossy(&confirmed.stderr)
    );
    let audit = store.get_gate_resolution(&task_id, 1).unwrap().unwrap();
    assert_eq!(audit.result, "operator-confirmed");
    assert_eq!(audit.resolved_by, "operator-a");
    assert!(audit.evidence.contains("已核对进程表"));
    assert!(!store
        .list_pending_gates()
        .unwrap()
        .iter()
        .any(|g| g.task_id == task_id));
    fs::remove_dir_all(&env.dir).ok();
}

/// rusqlite 参数小工具（测试用）。
fn rusqlite_params(task_id: &str) -> [&str; 1] {
    [task_id]
}

/// 已复现回归（Codex@8b62310）：活跃 daemon 持锁时，旧版 gate confirm
/// 退出 0、pending=false、audited=true。修复后：直接拒绝（非零退出），
/// 不改待确认标记、不写审计；停机后允许有证据的解封，审计恰好一条，
/// 重启后 B 只启动一次。
#[test]
fn gate_confirm_refuses_while_daemon_holds_lock_then_allows_after_stop() {
    let dir = temp_root("gc-lock");
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    // 造 pending gate（直接写库：本测试聚焦锁与审计，不跑业务链）。
    let db = instance.join("relay.sqlite3");
    let store = Store::open(&db).unwrap();
    {
        let conn = store.conn().unwrap();
        conn.execute_batch(
            "INSERT INTO plans (plan_id, project_id, goal, brief, base_oid, created_at, expires_at, status)
             VALUES ('plan_gc', 'proj', 'g', 'b', '0123456789abcdef0123456789abcdef01234567',
                     '2026-11-23T00:00:00Z', '2027-01-01T00:00:00Z', 'used');
             INSERT INTO tasks (task_id, plan_id, project_id, goal, base_oid, state, revision,
                                active_attempt, reason_code, created_at, updated_at)
             VALUES ('task_gc', 'plan_gc', 'proj', 'g', '0123456789abcdef0123456789abcdef01234567',
                     'interrupted', 2, 1, NULL, '2026-11-23T00:00:00Z', '2026-11-23T00:00:00Z');
             INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
             VALUES ('task_gc', 1, 'interrupted', 2, NULL, NULL, '2026-11-23T00:00:00Z');
             INSERT INTO dispatch_gate (task_id, attempt_no, state, reason, created_at, updated_at)
             VALUES ('task_gc', 1, 'pending', '测试夹具', '2026-11-23T00:00:00Z', '2026-11-23T00:00:00Z');",
        )
        .unwrap();
    }
    drop(store);
    let exe = env!("CARGO_BIN_EXE_relayctl");
    let gate_args = |note: &str| -> Vec<String> {
        vec![
            "gate".into(),
            "confirm".into(),
            "--instance-dir".into(),
            instance.display().to_string(),
            "--task-id".into(),
            "task_gc".into(),
            "--note".into(),
            note.into(),
            "--by".into(),
            "tester".into(),
            "--confirm-absent".into(),
        ]
    };

    // 活跃 daemon 持锁：拒绝（非零），标记与审计不变。
    let daemon = relay_daemon::start(&instance, "2026-11-23T00:00:00Z").unwrap();
    let refused = Command::new(exe)
        .args(gate_args("活跃 daemon 期间"))
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "活跃 daemon 持锁时必须拒绝: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    {
        let store = Store::open(&db).unwrap();
        assert!(
            store
                .list_pending_gates()
                .unwrap()
                .iter()
                .any(|g| g.task_id == "task_gc"),
            "拒绝后待确认标记不得改变"
        );
        assert!(
            store.get_gate_resolution("task_gc", 1).unwrap().is_none(),
            "拒绝后不得写审计"
        );
    }

    // 非法 --attempt：拒绝，不静默回退到 1。
    drop(daemon);
    let mut bad_args = gate_args("非法 attempt");
    bad_args.push("--attempt".into());
    bad_args.push("abc".into());
    let bad = Command::new(exe).args(&bad_args).output().unwrap();
    assert!(!bad.status.success(), "非法 --attempt 必须拒绝");
    let mut zero_args = gate_args("零 attempt");
    zero_args.push("--attempt".into());
    zero_args.push("0".into());
    let zero = Command::new(exe).args(&zero_args).output().unwrap();
    assert!(!zero.status.success(), "--attempt 0 必须拒绝");
    {
        let store = Store::open(&db).unwrap();
        assert!(store.get_gate_resolution("task_gc", 1).unwrap().is_none());
    }

    // 停机后：有证据的解封成功，审计恰好一条。
    let confirmed = Command::new(exe)
        .args(gate_args("停机后已核对进程表"))
        .output()
        .unwrap();
    assert!(
        confirmed.status.success(),
        "停机后应允许解封: {}",
        String::from_utf8_lossy(&confirmed.stderr)
    );
    let store = Store::open(&db).unwrap();
    assert!(store.list_pending_gates().unwrap().is_empty());
    let audit = store.get_gate_resolution("task_gc", 1).unwrap().unwrap();
    assert_eq!(audit.result, "operator-confirmed");
    assert_eq!(audit.resolved_by, "tester");
    // 幂等：重复解封返回同一审计，不新增。
    let again = Command::new(exe)
        .args(gate_args("重复解封"))
        .output()
        .unwrap();
    assert!(again.status.success(), "重复解封应幂等返回既有审计");
    let conn = store.conn().unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_resolutions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "审计恰好一条");
    drop(conn);
    drop(store);

    // 重启后：queued 的 B 只启动一次（gate 已解封 → 恢复领取）。
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO plans (plan_id, project_id, goal, brief, base_oid, created_at, expires_at, status)
             VALUES ('plan_gb', 'proj', 'g', 'b', '0123456789abcdef0123456789abcdef01234567',
                     '2026-11-23T00:00:00Z', '2027-01-01T00:00:00Z', 'used');
             INSERT INTO tasks (task_id, plan_id, project_id, goal, base_oid, state, revision,
                                active_attempt, reason_code, created_at, updated_at)
             VALUES ('task_gb', 'plan_gb', 'proj', 'g', '0123456789abcdef0123456789abcdef01234567',
                     'queued', 1, 1, NULL, '2026-11-23T00:00:00Z', '2026-11-23T00:00:00Z');
             INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
             VALUES ('task_gb', 1, 'queued', 1, NULL, NULL, '2026-11-23T00:00:00Z');
             INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
             VALUES ('task_gb', 1, 1, 'submitted', 'queued', NULL, '2026-11-23T00:00:00Z');",
        )
        .unwrap();
    }
    // "重启"：新 Store 连接 + 新 service（内存状态全新）领取两次，B 恰一次。
    let store = Store::open(&db).unwrap();
    let now = store.claim_next("2026-11-23T00:00:00Z").unwrap();
    assert!(now.is_some(), "解封后应恢复领取 B");
    assert_eq!(now.unwrap().task_id, "task_gb");
    let again = store.claim_next("2026-11-23T00:00:00Z").unwrap();
    assert!(again.is_none(), "B 只启动一次（无 queued 任务）");
    let task = store.get_task("task_gb").unwrap().unwrap();
    assert_eq!(task.state.to_string(), "preparing");
    fs::remove_dir_all(&dir).ok();
}

/// probe 受持久派发门约束（本批）：probe 可启动子进程，因此只在启动
/// 许可提交后运行，并把进程组记到待确认标记上。probe 超时且回收无法
/// 确认 → A interrupted、gate 保持 pending（带 pgid）、B 保持 queued；
/// 重启后仍不派发；探测确认退出后才带审计解封，B 恰好启动一次、
/// A 不重跑（worker 从未启动）。
#[test]
fn probe_under_dispatch_gate_blocks_across_restart_until_exit_confirmed() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mode = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let env = setup("probe-gate", None);
    let mut config = env.config.clone();
    config.adapter_fixture =
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/mock_adapter.sh"));
    config.expected_model = Some("fake-adapter-v1".to_owned());
    config.adapter_probe_timeout_ms = 500;
    config.group_exit_budget_ms = 300;
    config.reclaim_term_ms = 300;
    config.reclaim_kill_ms = 300;
    config.worker_timeout_ms = 5_000;
    let db = env.dir.join("instance/relay.sqlite3");
    let store: Arc<Store> = Arc::new(Store::open(&db).unwrap());
    let service = M1Service::with_group_control(
        store.clone(),
        config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );
    // 夹具：probe 挂起（等待上限触发）。
    std::env::set_var("ADAPTER_PROBE_HANG", "1");
    std::env::set_var("ADAPTER_HOLD_SECONDS", "30");
    std::env::set_var("ADAPTER_EMIT", "success");

    // A：领取 → 栅栏（gate pending）→ probe 超时 → 回收未确认 → interrupted。
    let task_a = submit_simple_id(&env, &service, "5e6f0000-0000-4000-8000-0000000000a1");
    let stop_a = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_a2 = stop_a.clone();
    let svc_a = service.clone();
    let handle_a = std::thread::spawn(move || svc_a.run_one_claim(&stop_a2));
    let started = std::time::Instant::now();
    let mut a_state = String::new();
    for _ in 0..300 {
        a_state = store.get_task(&task_a).unwrap().unwrap().state.to_string();
        if a_state == "interrupted" || a_state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    handle_a.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "probe 超时路径须在期限内返回: {:?}",
        started.elapsed()
    );
    assert_eq!(a_state, "interrupted", "回收未确认时 A 应 interrupted");
    let events = store.list_events(0, 100).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.task_id == task_a && e.kind == "spawned"),
        "worker 从未启动: {events:?}"
    );
    // gate 保持 pending，且带 probe 进程组（可核实的归属证据）。
    let gate = store
        .list_pending_gates()
        .unwrap()
        .into_iter()
        .find(|g| g.task_id == task_a)
        .expect("A 的 gate 应保持 pending");
    assert!(gate.pgid > 0, "gate 应记录 probe 进程组: {gate:?}");

    // B：同进程领取被阻断。
    let task_b = submit_simple_id(&env, &service, "5e6f0000-0000-4000-8000-0000000000b2");
    let stop = std::sync::atomic::AtomicBool::new(false);
    assert!(!service.run_one_claim(&stop), "gate 未确认时不得领取 B");
    assert_eq!(
        store.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "queued"
    );

    // 重启：Store 重开 + 新 service（内存全新）仍阻断。
    let store2: Arc<Store> = Arc::new(Store::open(&db).unwrap());
    let service2 = M1Service::with_group_control(
        store2.clone(),
        config.clone(),
        Arc::new(SwitchProbeControl { mode: mode.clone() }),
    );
    assert!(!service2.run_one_claim(&stop), "重启后仍不得领取 B");
    assert_eq!(
        store2.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "queued"
    );
    assert!(store2.get_launch_fence(&task_b, 1).unwrap().is_none());
    assert!(
        store2
            .list_events(0, 100)
            .unwrap()
            .iter()
            .all(|e| !(e.task_id == task_b && e.kind == "claim")),
        "重启前后 B 均不得有 claim"
    );

    // 确认退出（探测报组为空）→ 带审计解封 → 恢复领取，B 恰好一次。
    mode.store(0, std::sync::atomic::Ordering::SeqCst);
    std::env::set_var("ADAPTER_PROBE_HANG", "0");
    assert!(service2.run_one_claim(&stop), "确认退出后应恢复领取");
    let mut b_state = String::new();
    for _ in 0..300 {
        b_state = store2.get_task(&task_b).unwrap().unwrap().state.to_string();
        if b_state == "awaiting_review" || b_state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(b_state, "awaiting_review", "B 应完成整链");
    assert!(
        !store2
            .list_pending_gates()
            .unwrap()
            .iter()
            .any(|g| g.task_id == task_a),
        "确认退出后 A 的 gate 应已解封"
    );
    let audit = store2.get_gate_resolution(&task_a, 1).unwrap().unwrap();
    assert_eq!(audit.result, "confirmed-exit");
    assert_eq!(audit.resolved_by, "daemon");
    assert!(audit.evidence.contains("确认退出"), "{}", audit.evidence);
    let all = store2.list_events(0, 100).unwrap();
    let count = |task: &str, kind: &str| {
        all.iter()
            .filter(|e| e.task_id == task && e.kind == kind)
            .count()
    };
    assert_eq!(count(&task_a, "claim"), 1, "A 不重跑");
    assert_eq!(count(&task_b, "claim"), 1, "B 恰好启动一次");
    assert_eq!(
        store2.get_task(&task_a).unwrap().unwrap().state.to_string(),
        "interrupted"
    );

    std::env::remove_var("ADAPTER_PROBE_HANG");
    std::env::remove_var("ADAPTER_HOLD_SECONDS");
    std::env::remove_var("ADAPTER_EMIT");
    fs::remove_dir_all(&env.dir).ok();
}

/// 终止帧契约（本批）：重复终止帧与冲突终止帧均拒绝——任务 failed、
/// 不登记制品（无候选）、已确定结果不被后一帧覆盖。成功链路保留。
#[test]
fn duplicate_and_conflicting_terminal_frames_are_rejected_without_candidate() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for emit in [
        "duplicate_terminal",
        "conflicting_terminal",
        "rate_limited",
        "success",
    ] {
        let env = setup("term-frame", None);
        let mut config = env.config.clone();
        config.adapter_fixture =
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/mock_adapter.sh"));
        config.expected_model = Some("fake-adapter-v1".to_owned());
        config.worker_timeout_ms = 5_000;
        let db = env.dir.join("instance/relay.sqlite3");
        let store: Arc<Store> = Arc::new(Store::open(&db).unwrap());
        let service = M1Service::new(store.clone(), config);
        std::env::set_var("ADAPTER_EMIT", emit);
        std::env::set_var("ADAPTER_PROBE_HANG", "0");
        let task_id = submit_simple_id(
            &env,
            &service,
            match emit {
                "duplicate_terminal" => "7a8b0000-0000-4000-8000-0000000000d1",
                "conflicting_terminal" => "7a8b0000-0000-4000-8000-0000000000d2",
                "rate_limited" => "7a8b0000-0000-4000-8000-0000000000d4",
                _ => "7a8b0000-0000-4000-8000-0000000000d3",
            },
        );
        let stop = std::sync::atomic::AtomicBool::new(false);
        service.run_one_claim(&stop);
        let state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
        let artifact = store.get_artifact(&task_id, 1).unwrap();
        match emit {
            // 成功链路保留：到待验收并登记制品。
            "success" => {
                assert_eq!(state, "awaiting_review", "{emit}");
                assert!(artifact.is_some(), "{emit}: 成功链路应登记制品");
            }
            // 重复/冲突终止帧：拒绝、无候选。
            _ => {
                assert_eq!(state, "failed", "{emit}: 终止帧不合规应拒绝");
                assert!(artifact.is_none(), "{emit}: 不得登记制品");
                assert!(
                    !env.config
                        .managed_root
                        .join("artifacts")
                        .join(format!("{task_id}-1"))
                        .exists(),
                    "{emit}: 不得发布制品目录"
                );
            }
        }
        if emit == "rate_limited" {
            let events = store.list_events(0, 100).unwrap();
            assert_eq!(
                events
                    .iter()
                    .find(|e| e.task_id == task_id && e.kind == "worker_failed")
                    .and_then(|e| e.reason_code.as_deref()),
                Some("GO_QUOTA_EXHAUSTED"),
                "nonzero adapter exit must preserve its allowlisted terminal diagnostic"
            );
            assert_eq!(
                store
                    .get_task(&task_id)
                    .unwrap()
                    .unwrap()
                    .reason_code
                    .as_deref(),
                Some("GO_QUOTA_EXHAUSTED")
            );
            assert!(store.list_pending_gates().unwrap().is_empty());
        }
        std::env::remove_var("ADAPTER_EMIT");
        std::env::remove_var("ADAPTER_PROBE_HANG");
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// 已复现回归（Codex@bc3bfe8）：临时实例配置不存在的 worker，旧版 A
/// 进入 failed 却留下一条 pending 待确认标记，B 无法领取。修复后：明确
/// 未启动时失败状态、事件、解封与审计一起提交，B 恰好启动一次；A 不重跑。
#[test]
fn spawn_failure_commits_failure_and_release_together_then_b_starts_once() {
    let env = setup("spawn-fail", None);
    let db = env.dir.join("instance/relay.sqlite3");
    let store: Arc<Store> = Arc::new(Store::open(&db).unwrap());
    // 配置一个不存在的 worker（可信证据：进程从未启动）。
    let mut broken = env.config.clone();
    broken.mock_worker = env.dir.join("does-not-exist-worker.sh");
    broken.mock_worker_args.clear();
    let service = M1Service::new(store.clone(), broken);
    let task_a = submit_simple_id(&env, &service, "9f1e0000-0000-4000-8000-0000000000a1");
    let stop = std::sync::atomic::AtomicBool::new(false);
    service.run_one_claim(&stop);

    // A：failed，且事件里有 spawn_failed。
    let a = store.get_task(&task_a).unwrap().unwrap();
    assert_eq!(
        a.state.to_string(),
        "failed",
        "不存在的 worker 应使 A failed"
    );
    let events = store.list_events(0, 100).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.task_id == task_a && e.kind == "spawn_failed"),
        "{events:?}"
    );
    // 关键：不得留下 pending 待确认标记（旧版在此失败）。
    let pending: Vec<_> = store
        .list_pending_gates()
        .unwrap()
        .into_iter()
        .filter(|g| g.task_id == task_a)
        .collect();
    assert!(
        pending.is_empty(),
        "明确未启动时不得留下 pending 待确认标记: {pending:?}"
    );
    // 解封留审计（attempt、证据、结果）。
    let audit = store
        .get_gate_resolution(&task_a, 1)
        .unwrap()
        .expect("未启动的解封应有审计记录");
    assert_eq!(audit.result, "never-started");
    assert!(!audit.evidence.is_empty(), "{}", audit.evidence);

    // B：换成可用的 worker 配置，必须恰好启动一次并跑完整链。
    let service_ok = M1Service::new(store.clone(), env.config.clone());
    let task_b = submit_simple_id(&env, &service_ok, "9f1e0000-0000-4000-8000-0000000000b2");
    let stop_b = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_b2 = stop_b.clone();
    let svc_b = service_ok.clone();
    let handle_b = std::thread::spawn(move || svc_b.run_one_claim(&stop_b2));
    let mut b_state = String::new();
    for _ in 0..400 {
        b_state = store.get_task(&task_b).unwrap().unwrap().state.to_string();
        if b_state == "awaiting_review" || b_state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    handle_b.join().unwrap();
    assert_eq!(b_state, "awaiting_review", "B 应跑完整链");
    let all = store.list_events(0, 100).unwrap();
    let count = |task: &str, kind: &str| {
        all.iter()
            .filter(|e| e.task_id == task && e.kind == kind)
            .count()
    };
    assert_eq!(count(&task_b, "claim"), 1, "B 恰好启动一次");
    assert_eq!(count(&task_a, "claim"), 1, "A 不重跑");
    assert!(
        store.get_artifact(&task_a, 1).unwrap().is_none(),
        "A 无制品"
    );
    assert!(
        store.get_artifact(&task_b, 1).unwrap().is_some(),
        "B 登记制品"
    );
    fs::remove_dir_all(&env.dir).ok();
}
