//! 首版自动链路契约（15/16）：两个不同的本地适配器跑通
//! 实现 → 本地验证 → 独立只读评审 → 可交付结果；评审退回自动返工一次，
//! 再验证、再评审，到上限即停。回归覆盖：返工一次后交付、到上限即停、
//! 评审不能写、旧补丁摘要不能放行、不合规报告需人工处理、验证失败不送
//! 评审、角色互换（编排器不写死）、资格不足的明确提示、关闭前台后仍能
//! 完成、重启不重复派发。临时 HOME/仓库/实例，无网络、无真实模型。

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relay_daemon::service::{M1Service, ServiceConfig};
use relay_git::sha256_hex;
use relay_store::Store;
use serde_json::{json, Value};

const T0: &str = "2026-11-23T00:00:00Z";

fn temp_root(name: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // 短名：真实 daemon 的 socket 路径受 macOS sun_path 限制。
    let base = if Path::new("/private/tmp").is_dir() {
        std::path::PathBuf::from("/private/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!(
        "aw{}{seq}{}",
        std::process::id(),
        nanos % 1_000_000
    ));
    let _ = name;
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "awf")
        .env("GIT_AUTHOR_EMAIL", "awf@example.invalid")
        .env("GIT_COMMITTER_NAME", "awf")
        .env("GIT_COMMITTER_EMAIL", "awf@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Env {
    dir: PathBuf,
    repo: PathBuf,
    managed: PathBuf,
    instance: PathBuf,
}

fn builtin_catalog() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock/catalog")
}

fn setup() -> Env {
    let dir = temp_root("");
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
    Env {
        dir,
        repo,
        managed,
        instance,
    }
}

/// 基础配置：空配置（未指定适配器）+ 内置离线目录；验证规则固定通过。
fn base_config(env: &Env, rule_exit: i32) -> ServiceConfig {
    let rule = env.dir.join(format!("rule-{rule_exit}.sh"));
    if !rule.exists() {
        fs::write(&rule, format!("#!/bin/sh\nexit {rule_exit}\n")).unwrap();
        fs::set_permissions(&rule, fs::Permissions::from_mode(0o755)).unwrap();
    }
    ServiceConfig::from_json(&json!({
        "repo_root": env.repo.display().to_string(),
        "managed_root": env.managed.display().to_string(),
        "adapter_catalog_dir": builtin_catalog().display().to_string(),
        "worker_timeout_ms": 10_000,
        "rules": [{"id": "mock_rule", "executable": rule.display().to_string(),
                   "argv": [], "timeout_ms": 5_000}],
    }))
    .unwrap()
}

fn open_store(env: &Env) -> Arc<Store> {
    Arc::new(Store::open(&env.instance.join("relay.sqlite3")).unwrap())
}

fn plan_and_submit(env: &Env, service: &M1Service, request_id: &str) -> (String, Value) {
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&env.repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let plan = service
        .dispatch(
            "relay_plan",
            Some(&json!({
                "project_id": "proj_awf", "goal": "自动链路离线样本", "brief": "修改 src/count.rs 并经独立评审",
                "write_scope": ["src/count.rs", "tests/count.rs"],
                "profile_id": "prof_auto",
                "verification_rule_ids": ["mock_rule"],
                "base_oid": head,
            })),
        )
        .unwrap();
    assert!(plan.get("error").is_none(), "relay_plan 失败: {plan}");
    let plan_id = plan["plan_id"].as_str().unwrap().to_owned();
    let task = service
        .dispatch(
            "relay_submit",
            Some(&json!({"plan_id": plan_id, "request_id": request_id})),
        )
        .unwrap();
    assert!(task.get("error").is_none(), "relay_submit 失败: {task}");
    (task["task"]["id"].as_str().unwrap().to_owned(), plan)
}

fn run_claim_to_end(service: &M1Service) {
    let stop = AtomicBool::new(false);
    service.run_one_claim(&stop);
}

fn wait_state(store: &Store, task_id: &str, want: &[&str]) -> String {
    let mut state = String::new();
    for _ in 0..600 {
        state = store.get_task(task_id).unwrap().unwrap().state.to_string();
        if want.contains(&state.as_str()) {
            return state;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    state
}

fn events_of(store: &Store, task_id: &str) -> Vec<(String, Option<String>)> {
    store
        .list_events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.task_id == task_id)
        .map(|e| (e.kind, e.reason_code))
        .collect()
}

fn count_kind(store: &Store, task_id: &str, kind: &str) -> usize {
    events_of(store, task_id)
        .iter()
        .filter(|(k, _)| k == kind)
        .count()
}

fn handoff_of(store: &Store, task_id: &str) -> Value {
    let record = store
        .get_artifact(task_id, 1)
        .unwrap()
        .expect("应有已登记制品");
    let text = fs::read_to_string(Path::new(&record.artifact_dir).join("handoff.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn assert_no_candidate(env: &Env, store: &Store, task_id: &str) {
    assert!(
        store.get_artifact(task_id, 1).unwrap().is_none(),
        "不得登记制品"
    );
    assert!(
        !env.managed
            .join("artifacts")
            .join(format!("{task_id}-1"))
            .exists(),
        "不得发布制品目录"
    );
    assert!(
        store
            .list_pending_gates()
            .unwrap()
            .iter()
            .all(|g| g.task_id != task_id),
        "失败收口后不得留下 pending 待确认标记"
    );
}

/// 快乐路径 + 一次返工：首轮留 TODO 被评审退回 → 返工 → 再验证 → 再评审
/// 通过 → 待验收；交付包含评审结论、返工次数与实际路由；重启不重复派发。
#[test]
fn auto_chain_reworks_once_then_delivers() {
    let env = setup();
    let mut config = base_config(&env, 0);
    config
        .implement_env
        .insert("IMPLEMENT_MODE".to_owned(), "rework-once".to_owned());
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config.clone());
    let (task_id, plan) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000001");
    // 预览说清各角色将用什么：两个不同的工具与模型身份。
    let adapters = &plan["resolved"]["adapters"];
    assert_eq!(adapters["template"], json!("implement_review"));
    assert_ne!(
        adapters["implement"]["identity"],
        adapters["review"]["identity"]
    );
    assert_ne!(adapters["implement"]["model"], adapters["review"]["model"]);
    assert_eq!(adapters["review"]["capabilities"], json!(["read_files"]));

    run_claim_to_end(&service);
    let state = wait_state(&store, &task_id, &["awaiting_review", "failed"]);
    assert_eq!(state, "awaiting_review");

    let events = events_of(&store, &task_id);
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    let position = |kind: &str, reason_contains: &str| {
        events
            .iter()
            .position(|(k, r)| k == kind && r.as_deref().unwrap_or("").contains(reason_contains))
            .unwrap_or_else(|| panic!("缺少事件 {kind}/{reason_contains}: {kinds:?}"))
    };
    // 轮次顺序：验证1 → 评审1(退回) → 返工 → 验证2 → 评审2(通过)。
    let v1 = position("verify_round", "round=1");
    let r1 = position("review_round", "round=1");
    let rejected = position("review_changes_requested", "round=1");
    let rework = position("rework_round", "round=2");
    let v2 = position("verify_round", "round=2");
    let r2 = position("review_round", "round=2");
    let passed = position("review_passed", "round=2");
    assert!(v1 < r1 && r1 < rejected && rejected < rework && rework < v2 && v2 < r2 && r2 < passed);
    assert_eq!(
        count_kind(&store, &task_id, "rework_round"),
        1,
        "只自动返工一次"
    );
    assert_eq!(count_kind(&store, &task_id, "claim"), 1, "A 只领取一次");

    // 交付：评审结论、返工次数、实际路由；两个摘要绑定。
    let handoff = handoff_of(&store, &task_id);
    let workflow = &handoff["workflow"];
    assert_eq!(workflow["rounds"], json!(2));
    assert_eq!(workflow["reworks"], json!(1));
    assert_eq!(workflow["review"]["verdict"], json!("pass"));
    assert_eq!(
        workflow["routing"]["implement"]["identity"],
        json!("fake-implementer")
    );
    assert_eq!(
        workflow["routing"]["review"]["identity"],
        json!("mock-reviewer")
    );
    let record = store.get_artifact(&task_id, 1).unwrap().unwrap();
    assert_eq!(
        workflow["review"]["candidate_diff_sha256"],
        json!(record.patch_sha256)
    );
    assert_eq!(
        workflow["review"]["test_receipt_sha256"],
        json!(record.receipt_sha256)
    );
    let stage_rows: Vec<_> = rusqlite::Connection::open(env.instance.join("relay.sqlite3"))
        .unwrap()
        .prepare(
            "SELECT round, stage, state, diff_sha256, receipt_sha256 FROM stage_runs
             WHERE task_id=?1 ORDER BY rowid",
        )
        .unwrap()
        .query_map([&task_id], |r| {
            Ok((
                r.get::<_, u32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stage_rows
            .iter()
            .map(|(n, k, s, _, _)| (*n, k.as_str(), s.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "implement", "succeeded"),
            (1, "verify", "succeeded"),
            (1, "review", "needs_changes"),
            (2, "implement", "succeeded"),
            (2, "verify", "succeeded"),
            (2, "review", "succeeded")
        ]
    );
    assert_eq!(stage_rows[1].3, stage_rows[2].3);
    assert_eq!(stage_rows[1].4, stage_rows[2].4);
    assert_eq!(stage_rows[4].3, stage_rows[5].3);
    assert_eq!(stage_rows[4].4, stage_rows[5].4);
    assert_eq!(
        stage_rows[5].3.as_deref(),
        Some(record.patch_sha256.as_str())
    );
    assert_eq!(
        stage_rows[5].4.as_deref(),
        Some(record.receipt_sha256.as_str())
    );
    let patch = fs::read_to_string(Path::new(&record.artifact_dir).join("diff.patch")).unwrap();
    assert_eq!(
        sha256_hex(patch.as_bytes()),
        record.patch_sha256,
        "登记摘要 = 被评审的补丁"
    );
    assert!(!patch.contains("TODO"), "返工后缺陷已移除: {patch}");
    assert!(patch.contains("fixed in rework round 2"));
    // 人工验收：两个摘要与登记一致才放行（模型评审不冒充用户）。
    let accepted = service
        .dispatch(
            "relay_review",
            Some(&json!({
                "task_id": task_id, "attempt_no": 1,
                "expected_revision": store.get_task(&task_id).unwrap().unwrap().revision,
                "request_id": "a0000000-0000-4000-8000-000000000002",
                "verdict": "accept", "diff_sha256": record.patch_sha256,
                "test_sha256": record.receipt_sha256,
            })),
        )
        .unwrap();
    assert_eq!(accepted["task"]["state"], json!("completed"), "{accepted}");

    // 重启（重开库 + 新 service，内存全新）：没有可领取的任务，A 不重跑。
    let store2 = open_store(&env);
    let service2 = M1Service::new(store2.clone(), config);
    let stop = AtomicBool::new(false);
    assert!(!service2.run_one_claim(&stop), "重启后不得重复派发");
    assert_eq!(count_kind(&store2, &task_id, "claim"), 1);
    assert!(
        store2.list_pending_gates().unwrap().is_empty(),
        "gate 已解封"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// 到上限即停：评审每次都退回 → 返工一次 → 仍退回 → 停止；不产生候选。
#[test]
fn auto_chain_stops_at_rework_limit() {
    let env = setup();
    let mut config = base_config(&env, 0);
    config
        .implement_env
        .insert("IMPLEMENT_MODE".to_owned(), "rework-once".to_owned());
    config
        .review_env
        .insert("REVIEW_MODE".to_owned(), "always-reject".to_owned());
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config);
    let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000003");
    run_claim_to_end(&service);
    let state = wait_state(&store, &task_id, &["failed"]);
    assert_eq!(state, "failed");
    assert_eq!(count_kind(&store, &task_id, "review_round"), 2, "评审两轮");
    assert_eq!(
        count_kind(&store, &task_id, "rework_round"),
        1,
        "只返工一次"
    );
    assert_eq!(count_kind(&store, &task_id, "review_passed"), 0);
    let failed = events_of(&store, &task_id)
        .into_iter()
        .find(|(k, _)| k == "workflow_failed")
        .map(|(_, r)| r.unwrap_or_default())
        .unwrap();
    assert!(failed.contains("返工上限"), "{failed}");
    assert_no_candidate(&env, &store, &task_id);
    // 到上限后可经显式重试开始新 attempt（不自动重试）。
    fs::remove_dir_all(&env.dir).ok();
}

/// 返工次数为 0：首轮被退回即停。
#[test]
fn auto_chain_with_zero_rework_stops_after_first_rejection() {
    let env = setup();
    let mut config = base_config(&env, 0);
    config.max_rework = 0;
    config
        .implement_env
        .insert("IMPLEMENT_MODE".to_owned(), "rework-once".to_owned());
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config);
    let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000004");
    run_claim_to_end(&service);
    assert_eq!(wait_state(&store, &task_id, &["failed"]), "failed");
    assert_eq!(count_kind(&store, &task_id, "review_round"), 1);
    assert_eq!(count_kind(&store, &task_id, "rework_round"), 0);
    assert_no_candidate(&env, &store, &task_id);
    fs::remove_dir_all(&env.dir).ok();
}

/// 评审不能写：评审子进程写入受审候选 → 评审后候选变化 → 不放行、无候选。
#[test]
fn reviewer_writing_the_candidate_is_detected_and_blocks_delivery() {
    for mode in ["rogue-write", "rogue-write-reject"] {
        let env = setup();
        let store = open_store(&env);
        let probe_config = base_config(&env, 0);
        let service = M1Service::new(store.clone(), probe_config.clone());
        let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000005");
        // 评审者拿到 worktree 内文件路径并尝试写入（daemon 本不提供该路径）。
        let target = env
            .managed
            .join("worktrees")
            .join(&task_id)
            .join("attempt-1/src/count.rs");
        let mut config = probe_config;
        config
            .review_env
            .insert("REVIEW_MODE".to_owned(), mode.to_owned());
        config.review_env.insert(
            "REVIEW_ROGUE_TARGET".to_owned(),
            target.to_string_lossy().to_string(),
        );
        let service = M1Service::new(store.clone(), config);
        run_claim_to_end(&service);
        assert_eq!(wait_state(&store, &task_id, &["failed"]), "failed");
        assert_eq!(count_kind(&store, &task_id, "review_candidate_changed"), 1);
        assert_eq!(count_kind(&store, &task_id, "review_passed"), 0);
        assert_eq!(count_kind(&store, &task_id, "rework_round"), 0);
        assert_no_candidate(&env, &store, &task_id);
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// 旧补丁摘要不能放行：评审报告绑定的补丁/收据摘要与当前候选不符即无效。
#[test]
fn stale_digest_in_review_report_cannot_release_a_candidate() {
    for (mode, request) in [
        ("stale-digest", "a0000000-0000-4000-8000-000000000006"),
        ("wrong-receipt", "a0000000-0000-4000-8000-000000000007"),
    ] {
        let env = setup();
        let mut config = base_config(&env, 0);
        config
            .review_env
            .insert("REVIEW_MODE".to_owned(), mode.to_owned());
        let store = open_store(&env);
        let service = M1Service::new(store.clone(), config);
        let (task_id, _) = plan_and_submit(&env, &service, request);
        run_claim_to_end(&service);
        assert_eq!(wait_state(&store, &task_id, &["failed"]), "failed");
        assert_eq!(count_kind(&store, &task_id, "review_invalid"), 1, "{mode}");
        let reason = events_of(&store, &task_id)
            .into_iter()
            .find(|(k, _)| k == "review_invalid")
            .map(|(_, r)| r.unwrap_or_default())
            .unwrap();
        assert!(reason.contains("摘要"), "{mode}: {reason}");
        assert_eq!(count_kind(&store, &task_id, "review_passed"), 0, "{mode}");
        assert_no_candidate(&env, &store, &task_id);
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// 不合规报告一律需人工处理：不确定、结构无效、超限、结论帧缺失/重复、
/// 评审进程非成功终止——都不得报成功，也不自动返工。
#[test]
fn inconclusive_or_malformed_reports_need_handling_without_candidate() {
    let modes = [
        "inconclusive",
        "reject-no-finding",
        "bad-schema",
        "too-many-findings",
        "oversized",
        "no-verdict",
        "dup-verdict",
        "not-success",
    ];
    for (index, mode) in modes.iter().enumerate() {
        let env = setup();
        let mut config = base_config(&env, 0);
        config
            .review_env
            .insert("REVIEW_MODE".to_owned(), mode.to_string());
        let store = open_store(&env);
        let service = M1Service::new(store.clone(), config);
        let request = format!("a0000000-0000-4000-8000-{:012}", 0x10 + index);
        let (task_id, _) = plan_and_submit(&env, &service, &request);
        run_claim_to_end(&service);
        let state = wait_state(&store, &task_id, &["failed"]);
        assert_eq!(state, "failed", "{mode}");
        assert_eq!(count_kind(&store, &task_id, "review_passed"), 0, "{mode}");
        assert_eq!(
            count_kind(&store, &task_id, "rework_round"),
            0,
            "{mode}: 不自动返工"
        );
        assert_no_candidate(&env, &store, &task_id);
        fs::remove_dir_all(&env.dir).ok();
    }
}

/// pass 却附带阻断 finding：按退回处理，不报成功。
#[test]
fn pass_with_blocking_finding_is_treated_as_rejected() {
    let env = setup();
    let mut config = base_config(&env, 0);
    config
        .review_env
        .insert("REVIEW_MODE".to_owned(), "blocking-pass".to_owned());
    config.max_rework = 0;
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config);
    let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000020");
    run_claim_to_end(&service);
    assert_eq!(wait_state(&store, &task_id, &["failed"]), "failed");
    let changes = events_of(&store, &task_id)
        .into_iter()
        .find(|(k, _)| k == "review_changes_requested")
        .map(|(_, r)| r.unwrap_or_default())
        .unwrap();
    assert!(changes.contains("pass_with_blocking=true"), "{changes}");
    assert_no_candidate(&env, &store, &task_id);
    fs::remove_dir_all(&env.dir).ok();
}

/// 验证失败不自动送给评审（15）：展示收据即止，无评审轮次。
#[test]
fn verification_failure_is_not_sent_to_review() {
    let env = setup();
    let config = base_config(&env, 3);
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config);
    let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000021");
    run_claim_to_end(&service);
    assert_eq!(wait_state(&store, &task_id, &["failed"]), "failed");
    assert_eq!(count_kind(&store, &task_id, "verify_failed"), 1);
    assert_eq!(
        count_kind(&store, &task_id, "review_round"),
        0,
        "验证失败不得送评审"
    );
    assert_no_candidate(&env, &store, &task_id);
    fs::remove_dir_all(&env.dir).ok();
}

/// 角色互换（15）：编排器不写死具体适配器——换一组名字与身份都不同的
/// 目录项，仍能选出组合并跑通整链。
#[test]
fn orchestrator_is_not_hardcoded_to_specific_adapters() {
    let env = setup();
    // 另一组目录项：不同文件名、工具标识与模型身份，包装同一对夹具。
    let catalog = env.dir.join("catalog-swapped");
    fs::create_dir_all(&catalog).unwrap();
    let mock_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock");
    let author = catalog.join("author-tool.sh");
    fs::write(
        &author,
        format!(
            "#!/bin/sh\nADAPTER_IDENTITY=author-tool ADAPTER_ACTUAL_MODEL=author-model exec '{}' \"$@\"\n",
            mock_dir.join("mock_adapter.sh").display()
        ),
    )
    .unwrap();
    let auditor = catalog.join("auditor-tool.sh");
    fs::write(
        &auditor,
        format!(
            "#!/bin/sh\nREVIEWER_MODEL=auditor-model exec '{}' \"$@\"\n",
            mock_dir.join("mock_reviewer.sh").display()
        ),
    )
    .unwrap();
    for path in [&author, &auditor] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut config = base_config(&env, 0);
    config.adapter_catalog_dir = Some(catalog);
    let store = open_store(&env);
    let service = M1Service::new(store.clone(), config);
    let (task_id, plan) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000022");
    assert_eq!(
        plan["resolved"]["adapters"]["implement"]["identity"],
        json!("author-tool")
    );
    assert_eq!(
        plan["resolved"]["adapters"]["implement"]["model"],
        json!("author-model")
    );
    assert_eq!(
        plan["resolved"]["adapters"]["review"]["model"],
        json!("auditor-model")
    );
    run_claim_to_end(&service);
    assert_eq!(
        wait_state(&store, &task_id, &["awaiting_review", "failed"]),
        "awaiting_review"
    );
    let workflow = &handoff_of(&store, &task_id)["workflow"];
    assert_eq!(
        workflow["routing"]["implement"]["identity"],
        json!("author-tool")
    );
    assert_eq!(
        workflow["routing"]["review"]["model"],
        json!("auditor-model")
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// 资格不足时明确提示（16）：预览不返回可提交 plan，逐项给出稳定原因；
/// 派发路径同样不会启动任何 worker。
#[test]
fn insufficient_qualification_is_reported_with_stable_reasons() {
    let mock_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mock");
    let wrapper = |dir: &Path, name: &str, body: String| -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let implementer = format!(
        "ADAPTER_ACTUAL_MODEL=shared-model exec '{}' \"$@\"\n",
        mock_dir.join("mock_adapter.sh").display()
    );
    // (目录内容描述, 目录, 期望原因片段)
    let reviewer_same_model = format!(
        "REVIEWER_MODEL=shared-model exec '{}' \"$@\"\n",
        mock_dir.join("mock_reviewer.sh").display()
    );
    let reviewer_writable = format!(
        "REVIEWER_CAPABILITIES=read_files,edit_files exec '{}' \"$@\"\n",
        mock_dir.join("mock_reviewer.sh").display()
    );
    let reviewer_unknown = format!(
        "REVIEWER_MODEL= exec '{}' \"$@\"\n",
        mock_dir.join("mock_reviewer.sh").display()
    );
    let cases: Vec<(&str, PathBuf, Vec<&str>)> = {
        let mut v = Vec::new();
        let empty = temp_root("");
        v.push(("空目录", empty, vec!["没有可执行条目"]));
        let only_impl = temp_root("");
        wrapper(&only_impl, "impl.sh", implementer.clone());
        v.push(("只有实现者", only_impl, vec!["没有同时满足", "无评审角色"]));
        let same_model = temp_root("");
        wrapper(&same_model, "a-impl.sh", implementer.clone());
        wrapper(&same_model, "b-rev.sh", reviewer_same_model.clone());
        v.push(("同一模型", same_model, vec!["不独立", "同一模型"]));
        let writable = temp_root("");
        wrapper(&writable, "impl.sh", implementer.clone());
        wrapper(&writable, "rev.sh", reviewer_writable.clone());
        v.push(("评审可写", writable, vec!["评审必须只读", "edit_files"]));
        let unknown = temp_root("");
        wrapper(&unknown, "impl.sh", implementer.clone());
        wrapper(&unknown, "rev.sh", reviewer_unknown.clone());
        v.push(("评审身份未核实", unknown, vec!["模型身份未核实"]));
        v
    };
    for (label, catalog, expected) in &cases {
        let env = setup();
        let mut config = base_config(&env, 0);
        config.adapter_catalog_dir = Some(catalog.clone());
        let store = open_store(&env);
        let service = M1Service::new(store.clone(), config.clone());
        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&env.repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let reply = service
            .dispatch(
                "relay_plan",
                Some(&json!({
                    "project_id": "proj_awf", "goal": "g", "brief": "b",
                    "write_scope": ["src/count.rs"], "profile_id": "prof_auto",
                    "verification_rule_ids": ["mock_rule"], "base_oid": head,
                })),
            )
            .unwrap();
        let error = &reply["error"];
        assert_eq!(
            error["code"],
            json!("ROUTE_UNAVAILABLE"),
            "{label}: {reply}"
        );
        assert_eq!(error["action"], json!("check_adapters"), "{label}");
        let message = error["message"].as_str().unwrap();
        for fragment in expected {
            assert!(
                message.contains(fragment),
                "{label}: 提示应含 {fragment:?}: {message}"
            );
        }
        // 未返回可提交 plan：没有登记任何计划。
        let count: i64 = store
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{label}: 资格不足不得返回可提交 plan");
        fs::remove_dir_all(&env.dir).ok();
    }
    for (_, catalog, _) in &cases {
        let _ = fs::remove_dir_all(catalog);
    }
}

/// 真实 daemon：提交后关闭前台连接，链路仍自动完成；重启 daemon 后不重复
/// 派发，待验收结果与评审结论可经业务 API 读到。
#[test]
fn foreground_closed_chain_completes_and_restart_does_not_redispatch() {
    use relay_daemon::ipc::IpcConfig;
    let env = setup();
    let mut config = base_config(&env, 0);
    config
        .implement_env
        .insert("IMPLEMENT_MODE".to_owned(), "rework-once".to_owned());
    let mut daemon = relay_daemon::start_m1(
        &env.instance,
        T0,
        IpcConfig {
            total_deadline_ms: 60_000,
            ..IpcConfig::default()
        },
        config.clone(),
    )
    .unwrap();
    daemon.serve_background();
    let first_socket = daemon.socket_path().to_owned();
    let call = |method: &str, params: Value| -> Value { rpc(&first_socket, method, params) };
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&env.repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let plan = call(
        "relay_plan",
        json!({
            "project_id": "proj_awf", "goal": "自动链路离线样本", "brief": "修改 src/count.rs 并经独立评审",
            "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_auto",
            "verification_rule_ids": ["mock_rule"], "base_oid": head,
        }),
    );
    assert_eq!(
        plan["resolved"]["adapters"]["review"]["identity"],
        json!("mock-reviewer")
    );
    let task = call(
        "relay_submit",
        json!({"plan_id": plan["plan_id"], "request_id": "a0000000-0000-4000-8000-000000000030"}),
    );
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();
    // 前台连接已随每次调用关闭；这里不再持有任何连接，等待链路自行完成。
    let mut state = String::new();
    for _ in 0..800 {
        let status = call("relay_status", json!({"task_id": task_id}));
        state = status["task"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if state == "awaiting_review" || state == "failed" {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(state, "awaiting_review");
    let handoff_text = call(
        "relay_artifact",
        json!({"task_id": task_id, "attempt_no": 1, "kind": "handoff"}),
    )["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let handoff: Value = serde_json::from_str(&handoff_text).unwrap();
    assert_eq!(handoff["workflow"]["reworks"], json!(1));
    assert_eq!(handoff["workflow"]["review"]["verdict"], json!("pass"));
    let events = call("events.list", json!({"after_seq": 0, "limit": 100}));
    let review_rounds = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["task_id"] == json!(task_id) && e["kind"] == json!("review_round"))
        .count();
    assert_eq!(review_rounds, 2);

    // 重启 daemon（同实例）：恢复不重派，状态与制品保持。
    drop(daemon);
    let mut restarted = relay_daemon::start_m1(
        &env.instance,
        "2026-11-23T01:00:00Z",
        IpcConfig::default(),
        config,
    )
    .unwrap();
    assert!(restarted.recovered().is_empty(), "无活动任务需恢复");
    restarted.serve_background();
    let status = rpc(
        restarted.socket_path(),
        "events.list",
        json!({"after_seq": 0, "limit": 100}),
    );
    let claims = status["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["task_id"] == json!(task_id) && e["kind"] == json!("claim"))
        .count();
    assert_eq!(claims, 1, "重启后不得重复派发");
    drop(restarted);
    fs::remove_dir_all(&env.dir).ok();
}

/// 本机 RPC 客户端：每连接 hello + 至多一条业务请求（04），返回 result。
fn rpc(socket: &Path, method: &str, params: Value) -> Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1}})
    )
    .unwrap();
    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
    let confirm: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(confirm["result"]["version"], json!(1));
    writeln!(
        stream,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":method,"params":params})
    )
    .unwrap();
    line.clear();
    std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
    serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
}

/// 业务错误结构可达（供上层区分）。
#[test]
fn biz_error_shape_is_stable() {
    let error = relay_daemon::service::BizError::new("ROUTE_UNAVAILABLE", "m", "check_adapters");
    let value = error.to_json();
    assert_eq!(value["error"]["code"], json!("ROUTE_UNAVAILABLE"));
    assert_eq!(value["schema_version"], json!(1));
}

/// 可重跑的首版离线演示入口：五个场景各自满足通过判据（退出码 0）。
#[test]
fn demo_auto_command_runs_all_scenarios() {
    let exe = env!("CARGO_BIN_EXE_relayctl");
    let base = temp_root("");
    for (scenario, must_contain, must_not_contain) in [
        (
            "rework-once",
            vec![
                "终态 awaiting_review",
                "返工 1 次",
                "人工验收：completed",
                "重启后派发次数：1",
            ],
            vec!["TODO"],
        ),
        (
            "clean",
            vec!["终态 awaiting_review", "返工 0 次", "人工验收：completed"],
            vec![],
        ),
        (
            "always-reject",
            vec!["终态 failed", "返工上限", "候选不可见"],
            vec!["人工验收"],
        ),
        (
            "rogue-write",
            vec!["终态 failed", "候选发生变化", "候选不可见"],
            vec!["人工验收"],
        ),
        (
            "stale-digest",
            vec!["终态 failed", "摘要与当前候选不符", "候选不可见"],
            vec!["人工验收"],
        ),
    ] {
        let output = Command::new(exe)
            .arg("demo-auto")
            .arg("--scenario")
            .arg(scenario)
            .arg("--work-dir")
            .arg(&base)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            output.status.success(),
            "{scenario}: exit={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status.code()
        );
        for fragment in must_contain {
            assert!(
                stdout.contains(fragment),
                "{scenario}: 输出应含 {fragment:?}:\n{stdout}"
            );
        }
        for fragment in must_not_contain {
            assert!(
                !stdout.contains(fragment),
                "{scenario}: 输出不应含 {fragment:?}:\n{stdout}"
            );
        }
        assert!(
            stdout.contains(&format!("demo-auto[{scenario}]：通过")),
            "{stdout}"
        );
    }
    let _ = fs::remove_dir_all(&base);
}

/// MCP 桥只转发已公布的 8 个工具（04：不向用户转发原始 RPC 管理接口）。
/// 经 tools/call 传任意方法名必须被桥以 JSON-RPC 层错误拒绝，而不是转发
/// 给 daemon 再包成 MCP 业务结果。
#[test]
fn mcp_bridge_refuses_to_forward_unadvertised_methods() {
    let env = setup();
    let mut daemon = relay_daemon::start(&env.instance, T0).unwrap();
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
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    {
        writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}})).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
        assert!(
            serde_json::from_str::<Value>(&line).unwrap()["result"]["protocolVersion"]
                == json!("2025-06-18")
        );
        // 通知帧没有回复：只发送，不读取。
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
    }
    let mut roundtrip = |value: &Value| -> Value {
        writeln!(stdin, "{value}").unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    };
    let mut replies = Vec::new();
    for (index, method) in [
        "events.list",
        "ui.overview",
        "relay_overview",
        "gate.confirm",
    ]
    .iter()
    .enumerate()
    {
        let reply = roundtrip(&json!({
            "jsonrpc":"2.0","id":10 + index as u64,"method":"tools/call",
            "params":{"name": method, "arguments": {}},
        }));
        // 拒绝 = JSON-RPC 层错误，且没有"被转发后由 daemon 返回"的业务结果。
        let refused = reply.get("error").is_some() && reply.get("result").is_none();
        replies.push(((*method).to_string(), refused, reply));
    }
    let advertised = roundtrip(&json!({
        "jsonrpc":"2.0","id":99,"method":"tools/call",
        "params":{"name":"relay_status","arguments":{"task_id":"task_none"}},
    }));
    drop(stdin);
    let _ = child.wait();
    drop(daemon);
    for (method, refused, reply) in &replies {
        assert!(*refused, "MCP 桥不得转发未公布的方法 {method}：{reply}");
    }
    assert!(
        advertised.get("result").is_some(),
        "已公布工具应正常转发: {advertised}"
    );
    fs::remove_dir_all(&env.dir).ok();
}

/// 任务窗口的只读总览：服务状态与版本、派发门、最近任务；待验收任务带
/// 评审结论与两个摘要（经完整性门读取）。
#[test]
fn ui_overview_reports_service_tasks_and_review_results() {
    let env = setup();
    let mut config = base_config(&env, 0);
    config
        .implement_env
        .insert("IMPLEMENT_MODE".to_owned(), "rework-once".to_owned());
    let mut daemon = relay_daemon::start_m1(&env.instance, T0, Default::default(), config).unwrap();
    daemon.serve_background();
    let first = daemon.socket_path().to_owned();
    let idle = rpc(&first, "ui.overview", json!({}));
    assert_eq!(idle["service"]["state"], json!("ready"));
    assert_eq!(idle["service"]["dispatch_gate"]["pending"], json!(0));
    assert_eq!(idle["service"]["versions"]["business"], json!("relay.v1"));
    assert_eq!(
        idle["service"]["versions"]["db_schema"],
        json!(relay_store::SCHEMA_VERSION)
    );
    assert_eq!(idle["tasks"], json!([]));

    let plan = rpc(
        &first,
        "relay_plan",
        json!({
            "project_id": "proj_awf", "goal": "窗口总览样本", "brief": "b",
            "write_scope": ["src/count.rs", "tests/count.rs"], "profile_id": "prof_auto",
            "verification_rule_ids": ["mock_rule"], "base_oid": head_of(&env.repo),
        }),
    );
    let task = rpc(
        &first,
        "relay_submit",
        json!({"plan_id": plan["plan_id"], "request_id": "a0000000-0000-4000-8000-000000000040"}),
    );
    let task_id = task["task"]["id"].as_str().unwrap().to_owned();
    let mut view = Value::Null;
    for _ in 0..800 {
        view = rpc(&first, "ui.overview", json!({}));
        if view["tasks"][0]["state"] == json!("awaiting_review") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let item = &view["tasks"][0];
    assert_eq!(item["id"], json!(task_id));
    assert_eq!(item["goal"], json!("窗口总览样本"));
    assert_eq!(item["review"]["verdict"], json!("pass"));
    assert_eq!(item["review"]["reworks"], json!(1));
    assert_eq!(
        item["review"]["reviewer"]["identity"],
        json!("mock-reviewer")
    );
    assert!(item["review"]["candidate_diff_sha256"]
        .as_str()
        .is_some_and(|s| s.len() == 64));
    // 篡改已登记补丁：窗口如实显示制品不符，不当作可验收结果。
    let record = open_store(&env).get_artifact(&task_id, 1).unwrap().unwrap();
    let patch = Path::new(&record.artifact_dir).join("diff.patch");
    let text = fs::read_to_string(&patch).unwrap();
    fs::write(&patch, format!("{text}// tampered\n")).unwrap();
    let tampered = rpc(&first, "ui.overview", json!({}));
    assert_eq!(
        tampered["tasks"][0]["review"]["error"],
        json!("ARTIFACT_INVALID")
    );
    drop(daemon);
    fs::remove_dir_all(&env.dir).ok();
}

fn head_of(repo: &Path) -> String {
    String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned()
}

#[test]
fn cancel_reaps_verifier_and_descendants_before_finishing() {
    use relay_daemon::service::{GroupControl, RealGroupControl};
    for template in ["implement_review", "implementation_only"] {
        let env = setup();
        let mut config = base_config(&env, 0);
        config.workflow_template = template.to_owned();
        config.reclaim_term_ms = 50;
        config.reclaim_kill_ms = 1500;
        let marker = env.dir.join("verifier-started");
        fs::write(
            env.dir.join("rule-0.sh"),
            format!(
                "#!/bin/sh\ntrap '' TERM\n/bin/sleep 15 &\nprintf '%s %s' \"$$\" \"$!\" > '{}'\nwait\n",
                marker.display()
            ),
        )
        .unwrap();
        let store = open_store(&env);
        let service = M1Service::new(store.clone(), config);
        let (task_id, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000079");
        let pgid = std::thread::scope(|scope| {
            let worker = scope.spawn(|| run_claim_to_end(&service));
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let pgid = loop {
                let persisted = store.list_pending_gates().unwrap().first().map(|g| g.pgid);
                if let (Some(pgid), Ok(marker)) = (persisted, fs::read_to_string(&marker)) {
                    let marker: Vec<i32> = marker
                        .split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect();
                    if marker.len() == 2 && pgid == marker[0] && pgid > 0 {
                        break pgid;
                    }
                }
                assert!(std::time::Instant::now() < deadline, "验证规则未登记进程组");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(RealGroupControl.probe(pgid).unwrap());
            let result = service.dispatch("relay_cancel", Some(&json!({
                "task_id": task_id, "expected_revision": store.get_task(&task_id).unwrap().unwrap().revision,
                "request_id": "a0000000-0000-4000-8000-000000000080",
            }))).unwrap();
            assert!(
                matches!(
                    result["task"]["state"].as_str(),
                    Some("cancelling" | "cancelled")
                ),
                "{result}"
            );
            worker.join().unwrap();
            pgid
        });
        let still_running = RealGroupControl.probe(pgid).unwrap();
        if still_running {
            let _ = RealGroupControl.kill(pgid);
        }
        assert!(!still_running, "{template}：取消后仍有验证进程");
        assert_eq!(
            store.get_task(&task_id).unwrap().unwrap().state.to_string(),
            "cancelled"
        );
        assert_eq!(count_kind(&store, &task_id, "review_round"), 0);
        assert!(!store.has_active_stage(&task_id, 1).unwrap());
        assert!(store.list_pending_gates().unwrap().is_empty());
        assert_no_candidate(&env, &store, &task_id);
        fs::remove_dir_all(&env.dir).ok();
    }
}

#[test]
fn unverifiable_rule_exit_remains_blocked_across_restart() {
    use relay_daemon::service::{GroupControl, RealGroupControl};
    struct Unclear(PathBuf);
    impl Unclear {
        fn matches(&self, pgid: i32) -> bool {
            let expected = fs::read_to_string(&self.0)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            Some(pgid) == expected
        }
    }
    impl GroupControl for Unclear {
        fn term(&self, pgid: i32) -> std::io::Result<()> {
            RealGroupControl.term(pgid)
        }
        fn kill(&self, pgid: i32) -> std::io::Result<()> {
            RealGroupControl.kill(pgid)
        }
        fn probe(&self, pgid: i32) -> std::io::Result<bool> {
            if self.matches(pgid) {
                return Err(std::io::Error::other("injected probe error"));
            }
            RealGroupControl.probe(pgid)
        }
    }
    let env = setup();
    let mut config = base_config(&env, 0);
    config.reclaim_term_ms = 50;
    config.reclaim_kill_ms = 50;
    let marker = env.dir.join("verify-unclear");
    fs::write(
        env.dir.join("rule-0.sh"),
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    let store = open_store(&env);
    let service =
        M1Service::with_group_control(store.clone(), config, Arc::new(Unclear(marker.clone())));
    let (task_a, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000081");
    let (task_b, _) = plan_and_submit(&env, &service, "a0000000-0000-4000-8000-000000000082");
    run_claim_to_end(&service);
    assert_eq!(
        store.get_task(&task_a).unwrap().unwrap().state.to_string(),
        "interrupted"
    );
    assert_eq!(count_kind(&store, &task_a, "review_round"), 0);
    assert!(store.get_artifact(&task_a, 1).unwrap().is_none());
    assert!(!env
        .managed
        .join("artifacts")
        .join(format!("{task_a}-1"))
        .exists());
    let reopened = open_store(&env);
    reopened.recover(T0).unwrap();
    let gate = reopened.list_pending_gates().unwrap().pop().unwrap();
    assert_eq!(
        gate.pgid,
        fs::read_to_string(&marker).unwrap().parse::<i32>().unwrap()
    );
    assert!(
        reopened.claim_next(T0).unwrap().is_none(),
        "退出未确认不能领取 B"
    );
    assert_eq!(
        reopened
            .get_task(&task_b)
            .unwrap()
            .unwrap()
            .state
            .to_string(),
        "queued"
    );
    fs::remove_dir_all(&env.dir).ok();
}

#[test]
fn migrate_v8_instance_requires_lock_and_keeps_a_readable_backup() {
    let env = setup();
    let db = env.instance.join("relay.sqlite3");
    drop(open_store(&env));
    {
        let raw = rusqlite::Connection::open(&db).unwrap();
        raw.execute_batch(
            "DROP TABLE stage_runs; UPDATE meta SET value='8' WHERE key='schema_version';",
        )
        .unwrap();
    }
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_relayctl"))
            .arg("migrate")
            .arg("--instance-dir")
            .arg(&env.instance)
            .output()
            .unwrap()
    };
    let (lock, _) = relay_daemon::acquire_instance_lock(&env.instance).unwrap();
    let refused = run();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("被其他进程持有"), "{stderr}");
    let backup = env.instance.join("relay.sqlite3.pre-v9");
    assert!(!backup.exists());
    drop(lock);
    let result = run();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(Store::open(&db).is_ok());
    assert!(matches!(
        Store::open(&backup),
        Err(relay_store::StoreError::SchemaTooOld {
            found: 8,
            supported: 9
        })
    ));
    assert!(!run().status.success(), "不覆盖备份");
    fs::remove_dir_all(&env.dir).ok();
}

#[test]
fn registered_project_bounds_new_plans_and_queued_work_after_configuration_drift() {
    let env = setup();
    let store = open_store(&env);
    let repo = env.repo.canonicalize().unwrap();
    let raw = json!({
        "repo_root": repo, "managed_root": env.managed,
        "project_binding":{"project_id":"proj_selected","write_scope":["src/count.rs"]},
        "rules":[{"id":"check","executable":"/usr/bin/true","argv":[],"timeout_ms":1000}],
    });
    let service = M1Service::new(Arc::clone(&store), ServiceConfig::from_json(&raw).unwrap());
    let send = |method, value: Value| service.dispatch(method, Some(&value)).unwrap();
    let registration = send("ui.project", json!({}));
    assert_eq!(registration["project_id"], "proj_selected");
    let request = json!({
        "project_id": "proj_selected", "goal": "check the registered project", "brief":"fixed local inputs",
        "write_scope":["src/count.rs"], "profile_id":registration["profile_id"],
        "verification_rule_ids":["check"]
    });
    for (key, bad, code) in [
        ("project_id", json!("proj_other"), "NOT_FOUND"),
        ("profile_id", json!("prof_other"), "PLAN_STALE"),
        ("write_scope", json!(["src/"]), "INVALID_ARGUMENT"),
    ] {
        let mut invalid = request.clone();
        invalid[key] = bad;
        assert_eq!(send("relay_plan", invalid)["error"]["code"], code);
    }
    let plan = send("relay_plan", request.clone());
    assert!(plan["plan_id"].is_string(), "{plan}");
    let second = send("relay_plan", request);
    assert!(second["plan_id"].is_string(), "{second}");
    let submission =
        json!({"plan_id":plan["plan_id"],"request_id":"c33c1a31-27a7-4de9-8b54-45c28bbe6722"});
    let first = send("relay_submit", submission.clone());
    let task_id = first["task"]["id"].as_str().unwrap();
    assert_eq!(first["task"]["state"], "queued");

    let mut changed = raw;
    changed["rules"][0]["timeout_ms"] = 2000.into();
    let changed = M1Service::new(
        Arc::clone(&store),
        ServiceConfig::from_json(&changed).unwrap(),
    );
    let send = |method, value: Value| changed.dispatch(method, Some(&value)).unwrap();
    assert_eq!(send("relay_submit", submission), first);
    assert_eq!(
        send(
            "relay_submit",
            json!({
                "plan_id":second["plan_id"],"request_id":"14a0990e-598f-43c2-81a3-bb825bc7feee"
            })
        )["error"]["code"],
        "PLAN_STALE"
    );
    run_claim_to_end(&changed);
    assert_eq!(
        store.get_task(task_id).unwrap().unwrap().state,
        relay_core::State::Failed
    );
    assert!(!env.managed.join("worktrees").exists());
    assert!(store.list_pending_gates().unwrap().is_empty());
    let contents = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(contents.status.success() && contents.stdout.is_empty());
}

/// A10：gate 终态收敛到制品登记单一收口，消除 review_active 与
/// implementation_only 的不对称；成功登记后 gate 已解封且后续任务可立即领取，
/// 验证失败路径同样解封不留孤儿 pending。
#[test]
fn implementation_only_success_and_failure_converge_gate_release() {
    let env = setup();
    let store = open_store(&env);

    let mut cfg = base_config(&env, 0);
    cfg.workflow_template = relay_daemon::service::TEMPLATE_IMPLEMENTATION_ONLY.to_owned();
    let service = M1Service::new(Arc::clone(&store), cfg);

    let (task_a, _) = plan_and_submit(&env, &service, "a1000000-0000-4000-8000-000000000001");
    let (task_b, _) = plan_and_submit(&env, &service, "a1000000-0000-4000-8000-000000000002");

    run_claim_to_end(&service);
    assert_eq!(
        store.get_task(&task_a).unwrap().unwrap().state.to_string(),
        "awaiting_review",
        "implementation_only 成功路径应进入 awaiting_review"
    );
    assert!(
        store.get_artifact(&task_a, 1).unwrap().is_some(),
        "A 应已登记制品"
    );
    assert!(
        store.list_pending_gates().unwrap().is_empty(),
        "A 成功登记制品后必须已解封派发门"
    );

    // B 必须可立即领取并完成，证明 A 的 gate 未遗留阻断。
    run_claim_to_end(&service);
    assert_eq!(
        store.get_task(&task_b).unwrap().unwrap().state.to_string(),
        "awaiting_review",
        "B 应在 A 解封后顺利进入 awaiting_review"
    );
    assert!(store.list_pending_gates().unwrap().is_empty());

    // 验证失败路径：同样收口解封 gate，不出具制品，不阻塞后续任务。
    let mut fail_cfg = base_config(&env, 1);
    fail_cfg.workflow_template = relay_daemon::service::TEMPLATE_IMPLEMENTATION_ONLY.to_owned();
    let fail_service = M1Service::new(Arc::clone(&store), fail_cfg);
    let (task_c, _) = plan_and_submit(&env, &fail_service, "a1000000-0000-4000-8000-000000000003");
    run_claim_to_end(&fail_service);
    assert_eq!(
        store.get_task(&task_c).unwrap().unwrap().state.to_string(),
        "failed",
        "验证失败应进入 failed"
    );
    assert!(store.get_artifact(&task_c, 1).unwrap().is_none());
    assert!(
        store.list_pending_gates().unwrap().is_empty(),
        "验证失败后受控子进程已退出，不得遗留 pending gate"
    );

    fs::remove_dir_all(&env.dir).ok();
}
