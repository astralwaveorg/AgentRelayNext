//! M1 端到端契约测试（08 T04/T05/T07/T08/T10 核心负例）：固定样本经
//! M1Service（与 CLI/MCP/daemon 共用的业务契约）跑通；临时 HOME/仓库/
//! 实例；无网络、无真实模型。T03 的 MCP 发现经 relayctl mcp 子进程验证。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relay_daemon::service::{M1Service, ServiceConfig};
use relay_git::sha256_hex;
use relay_store::Store;
use serde_json::json;

const T0: &str = "2026-11-22T01:00:00Z";

struct Env {
    dir: PathBuf,
    config: ServiceConfig,
    head: String,
}

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
        "m1e-{name}-{}{seq}{}",
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
        .env("GIT_AUTHOR_NAME", "m1e2e")
        .env("GIT_AUTHOR_EMAIL", "m1e2e@example.invalid")
        .output()
        .expect("git 启动失败");
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn setup(name: &str, rule_body: &str, hold_worker: bool) -> Env {
    let dir = temp_root(name);
    let repo = dir.join("repo");
    let instance = dir.join("instance");
    let managed = dir.join("managed");
    for path in [&repo, &instance, &managed] {
        fs::create_dir_all(path).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;
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
    fs::write(&rule, format!("#!/bin/sh\n{}\n", rule_body)).unwrap();
    fs::set_permissions(&rule, fs::Permissions::from_mode(0o755)).unwrap();

    let config = ServiceConfig {
        project_binding: None,
        claude_review_fixture: None,
        opencode_review_fixture: None,
        live_routes: None,
        opencode_implement_fixture: None,
        claude_implement_fixture: None,
        repo_root: repo.clone(),
        managed_root: managed.clone(),
        mock_worker: manifest_dir().join("mock/mock_worker.sh"),
        mock_worker_args: if hold_worker {
            vec!["hold".to_owned()]
        } else {
            vec![]
        },
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
        worker_timeout_ms: 15_000,
    };
    Env { dir, config, head }
}

/// 登记 plan 并提交任务，返回 task_id。
fn plan_and_submit(env: &Env, service: &M1Service, request_id: &str) -> String {
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e", "goal": "修复块计数在空输入时的返回值",
                "brief": "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock", "verification_rule_ids": ["mock_rule"],
                "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": request_id})),
        )
        .unwrap();
    task["task"]["id"].as_str().unwrap().to_owned()
}

#[test]
fn queue_limit_rejects_new_work_but_allows_idempotent_replay() {
    let env = setup("queue-limit", "exit 0", false);
    let store = Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let first_request = "00000000-0000-4000-8000-000000000000".to_owned();
    let first_task = plan_and_submit(&env, &service, &first_request);
    let first_plan = store.get_task_plan_id(&first_task).unwrap().unwrap();
    for index in 1..20 {
        let request = format!("00000000-0000-4000-8000-{index:012}");
        plan_and_submit(&env, &service, &request);
    }
    assert_eq!(store.count_nonterminal_tasks().unwrap(), 20);

    // A successful replay is still allowed at the cap.
    let replay = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({
                "plan_id": first_plan,
                "request_id": first_request,
            })),
        )
        .unwrap();
    assert!(
        replay.get("error").is_none(),
        "idempotent replay must pass: {replay}"
    );

    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e", "goal": "第 21 个任务", "brief": "队列上限负例",
                "write_scope": ["src/count.rs"], "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"], "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap();
    let rejected = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({
                "plan_id": plan_id,
                "request_id": "00000000-0000-4000-8000-000000000021",
            })),
        )
        .unwrap();
    assert_eq!(rejected["error"]["code"], json!("QUEUE_FULL"), "{rejected}");
    assert_eq!(store.count_nonterminal_tasks().unwrap(), 20);
    fs::remove_dir_all(&env.dir).ok();
}

/// 跑完单个 attempt 并等待 awaiting_review；返回 (task_id, attempt_no, revision)。
fn chain_to_awaiting_review(env: &Env, service: &M1Service, store: &Store) -> (String, u32, u64) {
    let task_id = plan_and_submit(env, service, "3fa85f64-5717-4562-b3fc-2c963f66afa6");
    let stop = AtomicBool::new(false);
    for _ in 0..200 {
        if service.run_one_claim(&stop) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut state = String::new();
    for _ in 0..300 {
        let task = store.get_task(&task_id).unwrap().unwrap();
        state = task.state.to_string();
        if state == "awaiting_review" {
            return (task_id, task.active_attempt, task.revision);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    panic!("未进入 awaiting_review：{state}");
}

fn chain_to_awaiting_review_arc(
    env: &Env,
    service: &std::sync::Arc<M1Service>,
    store: &Store,
) -> (String, u32, u64) {
    chain_to_awaiting_review(env, service.as_ref(), store)
}

/// 端到端：plan → submit → worker 到 awaiting_review → 摘要核对 → accept。
#[test]
fn full_chain_reaches_completed_with_digests() {
    let env = setup("full", "exit 0", false);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());

    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e",
                "goal": "修复块计数在空输入时的返回值",
                "brief": "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock",
                "verification_rule_ids": ["mock_rule"],
                "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();

    let request_id = "3fa85f64-5717-4562-b3fc-2c963f66afa6";
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": request_id})),
        )
        .unwrap();
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();
    // T04：同 request 同参重送 → 同一 task。
    let again = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": request_id})),
        )
        .unwrap();
    assert_eq!(
        again["task"]["id"],
        json!(task_id),
        "幂等重送应返回同一任务"
    );

    // worker：领取 → 栅栏 → mock → 收据 → 发布登记。
    let stop = AtomicBool::new(false);
    for _ in 0..300 {
        if service.run_one_claim(&stop) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut state = String::new();
    for _ in 0..300 {
        state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
        if state == "awaiting_review" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(state, "awaiting_review", "整链应到 awaiting_review");

    // 只启动一次：事件链恰好一个 claim（T04）。
    let events = store.list_events(0, 100).unwrap();
    let claim_count = events.iter().filter(|e| e.kind == "claim").count();
    assert_eq!(claim_count, 1, "只允许一次领取: {events:?}");

    // 摘要核对：attempt/基线/补丁摘要一致（T05）。
    let task = store.get_task(&task_id).unwrap().unwrap();
    let record = store
        .get_artifact(&task_id, task.active_attempt)
        .unwrap()
        .expect("awaiting_review 应已登记制品");
    let manifest_text =
        fs::read_to_string(Path::new(&record.artifact_dir).join("manifest.json")).unwrap();
    let patch_text =
        fs::read_to_string(Path::new(&record.artifact_dir).join("diff.patch")).unwrap();
    let receipt_text =
        fs::read_to_string(Path::new(&record.artifact_dir).join("test_receipt.json")).unwrap();
    let patch_sha = sha256_hex(patch_text.as_bytes());
    assert!(manifest_text.contains(&format!(
        "\"attempt\":\"{task_id}-{}\"",
        task.active_attempt
    )));
    assert!(manifest_text.contains(&format!("\"base_oid\":\"{}\"", env.head)));
    let declared = manifest_text
        .split("\"patch_sha256\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap();
    assert_eq!(declared, patch_sha, "manifest 声明与实际补丁一致");
    assert_eq!(record.patch_sha256, patch_sha);
    assert_eq!(record.receipt_sha256, sha256_hex(receipt_text.as_bytes()));

    // T05：主 checkout HEAD/index/用户文件不变。
    let repo = &env.config.repo_root;
    let head_now = git(repo, &["rev-parse", "HEAD"]);
    assert_eq!(head_now, env.head, "主 checkout HEAD 不变");
    assert_eq!(
        git(repo, &["status", "--porcelain"]),
        "",
        "主 checkout 干净"
    );
    assert_eq!(
        fs::read_to_string(repo.join("src/count.rs")).unwrap(),
        "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n",
        "主 checkout 用户文件不变"
    );
    // 补丁在干净 checkout 可应用。
    let clean = env.dir.join("clean");
    fs::create_dir_all(&clean).unwrap();
    git(
        &clean,
        &["clone", "-q", repo.display().to_string().as_str(), "."],
    );
    let patch_path = env.dir.join("verify.patch");
    fs::write(&patch_path, &patch_text).unwrap();
    let apply = Command::new("git")
        .arg("-C")
        .arg(&clean)
        .args([
            "apply",
            "--check",
            patch_path.display().to_string().as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "补丁应可应用: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    // 验收 accept → completed。
    let reviewed = service
        .dispatch(
            "relay_review",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": task.active_attempt,
                "expected_revision": task.revision,
                "request_id": "8f14e45f-ceea-467f-a8aa-88fb4d4bd16b",
                "verdict": "accept", "diff_sha256": patch_sha,
                "test_sha256": record.receipt_sha256,
            })),
        )
        .unwrap();
    assert_eq!(reviewed["task"]["state"], json!("completed"));
    // T04：重启后重放同一 request 仍可得幂等结果（快照留 requests）。
    let replay = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": request_id})),
        )
        .unwrap();
    assert_eq!(replay["task"]["id"], json!(task_id));

    fs::remove_dir_all(&env.dir).ok();
}

/// T10：验证失败 → failed，不出具可验收制品、不登记。
#[test]
fn verification_failure_keeps_failed_without_artifact() {
    let env = setup("verifyfail", "exit 3", false);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock", "verification_rule_ids": ["mock_rule"],
                "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": "0f343b09-3124-49b9-a7ec-4f2ff51c6b52"})),
        )
        .unwrap();
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();
    let stop = AtomicBool::new(false);
    for _ in 0..200 {
        if service.run_one_claim(&stop) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut state = String::new();
    for _ in 0..200 {
        state = store.get_task(&task_id).unwrap().unwrap().state.to_string();
        if state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(state, "failed", "验证失败应 failed");
    assert!(
        store.get_artifact(&task_id, 1).unwrap().is_none(),
        "验证失败不登记制品"
    );
    assert!(
        !env.config
            .managed_root
            .join("artifacts")
            .join(format!("{task_id}-1"))
            .exists(),
        "验证失败不发布制品目录"
    );
    // 收据真实生成且 failed：T10 不接受"无收据"代替。
    let failed_events = store.list_events(0, 100).unwrap();
    assert!(
        failed_events
            .iter()
            .any(|e| e.kind == "verification_failed"),
        "{failed_events:?}"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// T07：取消运行中的 attempt → 回收确认归属的进程组 → cancelled；
/// 派发在后台线程（hold worker 挂起，等待 TERM）。
#[test]
fn cancel_running_attempt_confirms_stop() {
    let env = setup("cancel", "exit 0", true);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock", "verification_rule_ids": ["mock_rule"],
                "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": "5eb63bbb-0eb6-4df4-a32c-9e46cf9f9c02"})),
        )
        .unwrap();
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();

    // worker 在后台线程执行（hold worker 挂起）。
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let worker = service.clone();
    let handle = std::thread::spawn(move || worker.run_one_claim(&stop2));

    // 等待进入 running。
    let mut running = false;
    for _ in 0..200 {
        let t = store.get_task(&task_id).unwrap().unwrap();
        if t.state.to_string() == "running" {
            running = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(running, "应进入 running（hold worker）");

    // 取消：expected_revision 取当前值。
    let current = store.get_task(&task_id).unwrap().unwrap();
    let cancelled = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task_id,
                "expected_revision": current.revision,
                "request_id": "c9bf9e57-1685-4c89-bafb-ff5af830be8a",
            })),
        )
        .unwrap();
    assert_eq!(
        cancelled["task"]["state"],
        json!("cancelled"),
        "{cancelled}"
    );
    // worker 线程应随取消退出（进程组被回收）。
    handle.join().unwrap();
    // 终态：cancelled，不派下一 attempt。
    let final_task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(final_task.state.to_string(), "cancelled");
    let events = store.list_events(0, 100).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "cancel_requested"),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e.kind == "all_owned_workers_stopped"),
        "{events:?}"
    );
    // 重复取消：不产生第二个有效转换。
    let again = service.dispatch(
        "relay_cancel",
        Some(&serde_json::json!({
            "task_id": task_id, "expected_revision": final_task.revision,
            "request_id": "7f5ee2a5-9d1a-4d1c-9a4f-1e2b3c4d5e6f",
        })),
    );
    assert!(
        again.as_ref().unwrap().get("error").is_some(),
        "重复取消应拒绝"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// T08：服务在运行关键点停止 → 恢复归入 interrupted；未证实退出时
/// 重试被阻断、旧 attempt 不自动重跑、无新 attempt。
#[test]
fn interrupted_attempt_does_not_rerun_and_retry_blocked() {
    let env = setup("interrupt", "exit 0", true);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&serde_json::json!({
                "project_id": "proj_e2e", "goal": "g", "brief": "b",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_mock", "verification_rule_ids": ["mock_rule"],
                "base_oid": env.head,
            })),
        )
        .unwrap();
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&serde_json::json!({"plan_id": plan_id, "request_id": "b6ded87c-8f6e-4b9e-8e0b-2b73f61e5d31"})),
        )
        .unwrap();
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();

    // worker 后台运行，进入 running 后用停止标志中断（模拟 daemon 退出：
    // 不做状态推进，子进程被回收但不写"已取消"）。
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let worker = service.clone();
    let handle = std::thread::spawn(move || worker.run_one_claim(&stop2));
    for _ in 0..200 {
        let t = store.get_task(&task_id).unwrap().unwrap();
        if t.state.to_string() == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    handle.join().unwrap();

    // 显式恢复：running → interrupted。
    let recovered = store.recover("2026-11-22T02:00:00Z").unwrap();
    assert!(
        recovered.iter().any(|r| r.task_id == task_id),
        "{recovered:?}"
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "interrupted"
    );
    // 未证实退出：retry 阻断、不创建 attempt（04 relay_retry blocked）。
    let retry = service
        .dispatch(
            "relay_retry",
            Some(&serde_json::json!({
                "task_id": task_id, "plan_id": plan_id,
                "expected_revision": store.get_task(&task_id).unwrap().unwrap().revision,
                "request_id": "94c2d5d0-0e4f-4b0e-9f7c-0d8e2f3a4b5c",
            })),
        )
        .unwrap();
    assert!(
        retry.get("error").is_some(),
        "interrupted 未证实退出应拒绝重试: {retry}"
    );
    assert!(retry.to_string().contains("WRONG_STATE"), "{retry}");
    // 旧 attempt 不重跑：worker 领不到（无 queued）。
    let stop3 = AtomicBool::new(false);
    assert!(!service.run_one_claim(&stop3), "无 queued 任务可领取");
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().active_attempt,
        1,
        "不得创建新 attempt"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// T03（部分）：MCP 子进程握手 + 工具发现（固定顺序）+ tools/call 往返；
/// stdout 只输出协议（JSON 行）。真实 Codex 客户端握手单独验证，本机
/// 无第二 UID，跨 UID 门标未测。
#[test]
fn mcp_subprocess_handshake_and_tools_round_trip() {
    let env = setup("mcp", "exit 0", false);
    let instance = env.dir.join("instance");
    // 骨架 daemon（只读 status 替身）——MCP 的 relay_status 调用对任一
    // task_id 返回固定只读状态。
    let mut daemon = relay_daemon::start(&instance, T0).unwrap();
    daemon.serve_background();
    let socket = daemon.socket_path().to_owned();

    let exe = env!("CARGO_BIN_EXE_relayctl");
    let mut child = Command::new(exe)
        .arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let send = |stdin: &mut std::process::ChildStdin, v: &serde_json::Value| {
        use std::io::Write;
        writeln!(stdin, "{v}").unwrap();
    };
    let recv = |stdout: &mut std::process::ChildStdout| -> serde_json::Value {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(line.trim()).unwrap()
    };
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "relay-test", "version": "0"}},
        }),
    );
    let init = recv(&mut stdout);
    assert_eq!(
        init["result"]["protocolVersion"],
        json!("2025-06-18"),
        "{init}"
    );
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let tools = recv(&mut stdout);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "relay_plan",
            "relay_submit",
            "relay_status",
            "relay_list",
            "relay_artifact",
            "relay_cancel",
            "relay_retry",
            "relay_review"
        ],
        "工具清单顺序固定: {names:?}"
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "relay_status", "arguments": {"task_id": "task_any"}},
        }),
    );
    let call = recv(&mut stdout);
    assert_eq!(call["result"]["isError"], json!(false), "{call}");
    assert_eq!(
        call["result"]["structuredContent"]["schema_version"],
        json!(1),
        "{call}"
    );
    assert_eq!(
        call["result"]["structuredContent"]["state"],
        json!("ready"),
        "{call}"
    );
    // stdout 只输出协议：不写非 JSON 行（已由 recv 的 serde 解析证明）。
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    drop(daemon);
    fs::remove_dir_all(&env.dir).ok();
}

/// 已独立复现的回归（Codex@5de6a31）：awaiting_review 后改写已登记的
/// diff.patch，用原摘要调用 relay_review 曾错误进入 completed。完整性门
/// 必须拒绝且状态不变。
#[test]
fn tampered_registered_patch_rejects_review_without_state_change() {
    let env = setup("tamper-review", "exit 0", false);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let (task_id, attempt_no, revision) = chain_to_awaiting_review_arc(&env, &service, &store);
    let record = store.get_artifact(&task_id, attempt_no).unwrap().unwrap();
    let patch_path = Path::new(&record.artifact_dir).join("diff.patch");
    let original = fs::read_to_string(&patch_path).unwrap();
    // 篡改已登记的补丁。
    fs::write(&patch_path, format!("{original}\n// tampered\n")).unwrap();

    let reviewed = service.dispatch(
        "relay_review",
        Some(&serde_json::json!({
            "task_id": task_id, "attempt_no": attempt_no,
            "expected_revision": revision,
            "request_id": "2f6c3b1a-93e7-4f8e-9f1d-6a5b4c3d2e1f",
            "verdict": "accept",
            "diff_sha256": record.patch_sha256,
            "test_sha256": record.receipt_sha256,
        })),
    );
    let reply = reviewed.unwrap();
    assert!(reply.get("error").is_some(), "篡改制品必须拒绝: {reply}");
    assert!(reply.to_string().contains("ARTIFACT_INVALID"), "{reply}");
    // 状态不变：仍 awaiting_review，未推进 completed。
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state.to_string(),
        "awaiting_review",
        "验收被拒后状态不得变化"
    );
    // 事件未新增 review_accepted。
    let events = store.list_events(0, 100).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "review_accepted"),
        "{events:?}"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// relay_artifact 读取 handoff；篡改后读取同样拒绝（同一完整性门）。
#[test]
fn artifact_read_enforces_integrity_and_serves_handoff() {
    let env = setup("artifact-integrity", "exit 0", false);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let (task_id, attempt_no, _revision) = chain_to_awaiting_review_arc(&env, &service, &store);

    // handoff 可读且绑定 attempt/基线。
    let handoff = service
        .dispatch(
            "relay_artifact",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "handoff",
            })),
        )
        .unwrap();
    let content = handoff["content"].as_str().unwrap();
    assert!(
        content.contains(&format!("{task_id}-{attempt_no}")),
        "{content}"
    );
    assert!(content.contains(&env.head), "handoff 应绑定基线");

    // 篡改 test_receipt 后读取拒绝。
    let record = store.get_artifact(&task_id, attempt_no).unwrap().unwrap();
    let receipt_path = Path::new(&record.artifact_dir).join("test_receipt.json");
    let original = fs::read_to_string(&receipt_path).unwrap();
    fs::write(&receipt_path, format!("{original}\n// tampered\n")).unwrap();
    let reply = service
        .dispatch(
            "relay_artifact",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no, "kind": "test_receipt",
            })),
        )
        .unwrap();
    assert!(
        reply.get("error").is_some(),
        "篡改制品读取必须拒绝: {reply}"
    );
    assert!(reply.to_string().contains("ARTIFACT_INVALID"), "{reply}");
    fs::remove_dir_all(&env.dir).ok();
}

/// review/cancel 的 request_id 幂等：重放返回首次结果、不新增事件；
/// 同 ID 异参拒绝。
#[test]
fn cancel_and_review_request_replay() {
    let env = setup("replay", "exit 0", false);
    let store: std::sync::Arc<Store> =
        std::sync::Arc::new(Store::open(&env.dir.join("instance/relay.db")).unwrap());
    let service = M1Service::new(store.clone(), env.config.clone());
    let (task_id, attempt_no, revision) = chain_to_awaiting_review_arc(&env, &service, &store);
    let record = store.get_artifact(&task_id, attempt_no).unwrap().unwrap();

    // 验收 accept（request A）→ completed。
    let accept_id = "aaaaaaaa-1111-4111-8111-111111111111";
    let reviewed = service
        .dispatch(
            "relay_review",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no,
                "expected_revision": revision, "request_id": accept_id,
                "verdict": "accept", "diff_sha256": record.patch_sha256,
                "test_sha256": record.receipt_sha256,
            })),
        )
        .unwrap();
    assert_eq!(reviewed["task"]["state"], json!("completed"));
    let events_before = store.list_events(0, 100).unwrap().len();

    // 重放同 ID 同参：返回首次结果，不新增事件。
    let replay = service
        .dispatch(
            "relay_review",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no,
                "expected_revision": revision, "request_id": accept_id,
                "verdict": "accept", "diff_sha256": record.patch_sha256,
                "test_sha256": record.receipt_sha256,
            })),
        )
        .unwrap();
    assert_eq!(replay, reviewed, "重放返回首次结果");
    assert_eq!(
        store.list_events(0, 100).unwrap().len(),
        events_before,
        "重放不新增事件"
    );

    // 同 ID 异参：拒绝。
    let conflict = service
        .dispatch(
            "relay_review",
            Some(&serde_json::json!({
                "task_id": task_id, "attempt_no": attempt_no,
                "expected_revision": revision, "request_id": accept_id,
                "verdict": "reject", "diff_sha256": record.patch_sha256,
                "test_sha256": record.receipt_sha256,
            })),
        )
        .unwrap();
    assert!(
        conflict.to_string().contains("IDEMPOTENCY_CONFLICT"),
        "{conflict}"
    );

    // cancel 幂等：新任务 awaiting_review 前取消（queued → cancelled）。
    let task2 = plan_and_submit(&env, &service, "6f2a9c31-8b47-4e3d-9a12-5f6b7c8d9e0f");
    let cancel_id = "bbbbbbbb-2222-4222-8222-222222222222";
    let current_rev = store.get_task(&task2).unwrap().unwrap().revision;
    let cancelled = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task2, "expected_revision": current_rev,
                "request_id": cancel_id,
            })),
        )
        .unwrap();
    assert_eq!(
        cancelled["task"]["state"],
        json!("cancelled"),
        "{cancelled}"
    );
    let events_before2 = store.list_events(0, 100).unwrap().len();
    let replay2 = service
        .dispatch(
            "relay_cancel",
            Some(&serde_json::json!({
                "task_id": task2, "expected_revision": current_rev,
                "request_id": cancel_id,
            })),
        )
        .unwrap();
    assert_eq!(replay2, cancelled, "取消重放返回首次结果");
    assert_eq!(
        store.list_events(0, 100).unwrap().len(),
        events_before2,
        "重放不新增事件"
    );
    let _ = task_id;
    fs::remove_dir_all(&env.dir).ok();
}

/// e2e --work-dir 指向已有路径：在既有目录下新建独占子目录，已有内容
/// （哨兵文件）不受影响；e2e 自身在其子目录内完成整链。
#[test]
fn e2e_with_existing_work_dir_leaves_contents_untouched() {
    let base = temp_root("eb");
    let sentinel = base.join("sentinel.txt");
    fs::write(&sentinel, "do-not-touch").unwrap();
    let nested = base.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("nested-sentinel.txt"), "keep").unwrap();

    let exe = env!("CARGO_BIN_EXE_relayctl");
    let output = Command::new(exe)
        .arg("e2e")
        .arg("--work-dir")
        .arg(&base)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "e2e 应通过: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 哨兵原样保留。
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "do-not-touch");
    assert_eq!(
        fs::read_to_string(nested.join("nested-sentinel.txt")).unwrap(),
        "keep"
    );
    // 本次独占子目录已创建并保留（--keep 默认成功保留）。
    let entries: Vec<_> = fs::read_dir(&base)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let binding = e.file_name();
            let n = binding.to_string_lossy();
            n.starts_with('e') && n != "elsewhere"
        })
        .collect();
    assert!(!entries.is_empty(), "应在既有目录下新建本次独占子目录");
    fs::remove_dir_all(&base).ok();
}
