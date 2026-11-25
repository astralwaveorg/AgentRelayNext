//! WP1-A / WP1-B1 数据库契约测试：真实 SQLite 文件库、独立临时目录，不碰
//! 旧版数据。
//!
//! 覆盖：提交后逐表核对；两个独立连接并发提交只建一条任务（同 request_id
//! 去重与同 plan 消费竞争两种）；重开数据库后重送仍返回原任务（即使计划已
//! 过期，验证去重先于到期检查）；同 ID 换参数冲突；换 ID 重用已消费计划；
//! 未消费计划过期；事务中途注入写入失败完全回滚；WAL/耐久配置核对；
//! schema 更高版本拒绝打开且零持久写入；小数秒精确比较；闰秒拒绝；
//! 单阶段状态转换 CAS（持久化生效、并发竞争恰好一个成功、四类拒绝零写入、
//! 事件注入失败整体回滚、重放仍返回首次提交快照）。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use relay_core::{Event, State};
use relay_store::{
    migrate_v1_to_v2, migrate_v2_to_v3, migrate_v3_to_v4, migrate_v4_to_v5, migrate_v5_to_v6,
    migrate_v6_to_v7, migrate_v7_to_v8, migrate_v8_to_v9, sqlite_version, LaunchFenceInput,
    LaunchFenceOutcome, LaunchFenceRecord, PlanInput, RecoveryRecord, RequestResult, RetryInput,
    Store, StoreError, SubmitInput, SubmitOutcome, TaskSummary, TransitionInput, TransitionOutcome,
    OP_RETRY, OP_SUBMIT,
};
use rusqlite::Connection;

// 调用方传入的固定时间（UTC RFC3339；解析为时间点后比较，无 sleep）。
const T0: &str = "2026-11-22T01:00:00Z";
const T15: &str = "2026-11-22T01:15:00Z";
const T60: &str = "2026-11-22T02:00:00Z";
/// 远期过期时间，供需要跨越任意固定提交时间的计划使用。
const TFAR: &str = "2099-01-01T00:00:00Z";

const BASE_OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-store-wp1a-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn plan_input<'a>(plan_id: &'a str, created_at: &'a str, expires_at: &'a str) -> PlanInput<'a> {
    PlanInput {
        plan_id,
        project_id: "proj_f6a2",
        goal: "修复块计数在空输入时的返回值",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: BASE_OID,
        created_at,
        expires_at,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    }
}

fn submit_input<'a>(request_id: &'a str, plan_id: &'a str, now: &'a str) -> SubmitInput<'a> {
    SubmitInput {
        request_id,
        plan_id,
        now,
    }
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .conn()
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn scalar(store: &Store, sql: &str) -> String {
    store
        .conn()
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

#[test]
fn submit_writes_all_tables_atomically() {
    let dir = temp_dir("submit-atomic");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_ok", T0, T15)).unwrap();
    let outcome = store
        .submit(submit_input("req-0001", "plan_ok", T0))
        .unwrap();

    assert!(!outcome.deduplicated);
    assert_eq!(outcome.state, State::Queued);
    assert_eq!(outcome.revision, 1);
    assert_eq!(outcome.active_attempt, 1);
    assert_eq!(outcome.created_at, T0);
    let expected_json = format!(
        "{{\"schema_version\":1,\"task\":{{\"id\":\"{}\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}}}}",
        outcome.task_id
    );
    assert_eq!(outcome.result_json, expected_json);

    assert_eq!(count(&store, "plans"), 1);
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "attempts"), 1);
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "requests"), 1);

    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_ok'"),
        "used"
    );

    assert_eq!(
        store.get_task(&outcome.task_id).unwrap().unwrap(),
        TaskSummary {
            task_id: outcome.task_id.clone(),
            project_id: "proj_f6a2".into(),
            goal: "修复块计数在空输入时的返回值".into(),
            state: State::Queued,
            revision: 1,
            active_attempt: 1,
            reason_code: None,
            created_at: T0.into(),
            updated_at: T0.into(),
        }
    );

    let (state, revision, active_attempt, reason): (String, i64, i64, Option<String>) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT state, revision, active_attempt, reason_code FROM tasks WHERE task_id = ?1",
            rusqlite::params![outcome.task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), revision, active_attempt, reason),
        ("queued", 1, 1, None)
    );

    let (attempt_no, attempt_state, attempt_rev, started, ended, attempt_created): (
        i64,
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
    ) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT attempt_no, state, revision, started_at, ended_at, created_at
             FROM attempts WHERE task_id = ?1",
            rusqlite::params![outcome.task_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (attempt_no, attempt_state.as_str(), attempt_rev),
        (1, "queued", 1)
    );
    assert_eq!(started, None);
    assert_eq!(ended, None);
    assert_eq!(attempt_created, T0);

    let (seq, ev_task, ev_attempt, ev_rev, kind, ev_state): (
        i64,
        String,
        i64,
        i64,
        String,
        String,
    ) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT seq, task_id, attempt_no, revision, kind, state FROM events",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (
            seq,
            ev_task.as_str(),
            ev_attempt,
            ev_rev,
            kind.as_str(),
            ev_state.as_str()
        ),
        (1, outcome.task_id.as_str(), 1, 1, "submitted", "queued")
    );

    assert_eq!(
        store.get_request_result("req-0001").unwrap().unwrap(),
        RequestResult {
            request_id: "req-0001".into(),
            operation: OP_SUBMIT.into(),
            plan_id: "plan_ok".into(),
            task_id: outcome.task_id.clone(),
            result_json: expected_json,
            created_at: T0.into(),
        }
    );

    remove_dir(&dir);
}

#[test]
fn concurrent_duplicate_submits_create_one_task() {
    let dir = temp_dir("concurrent-dedup");
    let db = dir.join("relay.db");
    {
        let setup = Store::open(&db).unwrap();
        setup.put_plan(plan_input("plan_c", T0, T15)).unwrap();
    }
    let outcomes: Vec<SubmitOutcome> = std::thread::scope(|scope| {
        let handles = (0..2).map(|_| {
            let db = &db;
            scope.spawn(move || {
                let store = Store::open(db).unwrap();
                store.submit(submit_input("req-race", "plan_c", T0))
            })
        });
        handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect()
    });
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes.iter().filter(|o| !o.deduplicated).count(), 1);
    let task_ids: HashSet<&str> = outcomes.iter().map(|o| o.task_id.as_str()).collect();
    assert_eq!(task_ids.len(), 1);
    let jsons: HashSet<&str> = outcomes.iter().map(|o| o.result_json.as_str()).collect();
    assert_eq!(jsons.len(), 1);

    let store = Store::open(&db).unwrap();
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "requests"), 1);
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "attempts"), 1);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_c'"),
        "used"
    );

    remove_dir(&dir);
}

#[test]
fn concurrent_distinct_requests_on_one_plan_create_one_task() {
    let dir = temp_dir("concurrent-plan-race");
    let db = dir.join("relay.db");
    {
        let setup = Store::open(&db).unwrap();
        setup.put_plan(plan_input("plan_race", T0, T15)).unwrap();
    }
    let results: Vec<Result<SubmitOutcome, StoreError>> = std::thread::scope(|scope| {
        let handles = (0..2).map(|i| {
            let db = &db;
            let request_id = match i {
                0 => "req-a1",
                _ => "req-a2",
            };
            scope.spawn(move || {
                let store = Store::open(db).unwrap();
                store.submit(submit_input(request_id, "plan_race", T0))
            })
        });
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut created_task: Option<String> = None;
    let mut used_task: Option<String> = None;
    let mut ok_count = 0;
    for result in results {
        match result {
            Ok(outcome) => {
                ok_count += 1;
                created_task = Some(outcome.task_id);
            }
            Err(StoreError::PlanUsed {
                existing_task_id, ..
            }) => {
                used_task = existing_task_id;
            }
            Err(err) => panic!("意外的错误变体: {err:?}"),
        }
    }
    assert_eq!(ok_count, 1);
    assert_eq!(used_task, created_task, "PLAN_USED 应带回已建 task ID");

    let store = Store::open(&db).unwrap();
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "requests"), 1);
    assert_eq!(count(&store, "events"), 1);

    remove_dir(&dir);
}

#[test]
fn resubmit_after_reopen_returns_original_task_even_if_plan_expired() {
    let dir = temp_dir("reopen-replay");
    let db = dir.join("relay.db");
    let original = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_r", T0, T15)).unwrap();
        store
            .submit(submit_input("req-open", "plan_r", T0))
            .unwrap()
    };
    // 数据库重开；重送时计划已过期，但去重先于到期检查，仍返回原结果。
    let store = Store::open(&db).unwrap();
    let replay = store
        .submit(submit_input("req-open", "plan_r", T60))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.task_id, original.task_id);
    assert_eq!(replay.result_json, original.result_json);
    assert_eq!(replay.state, State::Queued);
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "requests"), 1);
    assert_eq!(count(&store, "events"), 1);

    remove_dir(&dir);
}

#[test]
fn same_request_id_with_different_params_conflicts() {
    let dir = temp_dir("idempotency-conflict");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_a", T0, T15)).unwrap();
    store.put_plan(plan_input("plan_b", T0, T15)).unwrap();
    let first = store.submit(submit_input("req-x", "plan_a", T0)).unwrap();

    let err = store
        .submit(submit_input("req-x", "plan_b", T0))
        .unwrap_err();
    match &err {
        StoreError::IdempotencyConflict {
            existing_task_id, ..
        } => {
            assert_eq!(existing_task_id.as_deref(), Some(first.task_id.as_str()));
        }
        other => panic!("意外的错误变体: {other:?}"),
    }
    assert_eq!(err.code(), "IDEMPOTENCY_CONFLICT");
    // plan_b 未被消费，数据库里仍只有一个任务。
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_b'"),
        "unused"
    );

    remove_dir(&dir);
}

#[test]
fn used_plan_rejects_new_request_id_and_reports_task() {
    let dir = temp_dir("plan-used");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_u", T0, T15)).unwrap();
    let first = store.submit(submit_input("req-1", "plan_u", T0)).unwrap();

    let err = store
        .submit(submit_input("req-2", "plan_u", T0))
        .unwrap_err();
    match &err {
        StoreError::PlanUsed {
            existing_task_id, ..
        } => {
            assert_eq!(existing_task_id.as_deref(), Some(first.task_id.as_str()));
        }
        other => panic!("意外的错误变体: {other:?}"),
    }
    assert_eq!(err.code(), "PLAN_USED");
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "requests"), 1);

    remove_dir(&dir);
}

#[test]
fn expired_plan_rejects_fresh_request_and_stays_usable_in_window() {
    let dir = temp_dir("plan-expired");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_e", T0, T15)).unwrap();

    let err = store
        .submit(submit_input("req-e", "plan_e", T60))
        .unwrap_err();
    match &err {
        StoreError::PlanExpired { plan_id } => assert_eq!(plan_id, "plan_e"),
        other => panic!("意外的错误变体: {other:?}"),
    }
    assert_eq!(err.code(), "PLAN_EXPIRED");
    // 失败提交不留任何行。
    assert_eq!(count(&store, "tasks"), 0);
    assert_eq!(count(&store, "attempts"), 0);
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "requests"), 0);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_e'"),
        "unused"
    );

    // 回到有效期内，同一请求可正常提交。
    let outcome = store.submit(submit_input("req-e", "plan_e", T0)).unwrap();
    assert!(!outcome.deduplicated);
    assert_eq!(count(&store, "tasks"), 1);

    remove_dir(&dir);
}

#[test]
fn injected_failure_rolls_back_whole_transaction() {
    let dir = temp_dir("fault-injection");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_f", T0, T15)).unwrap();

    // 事务中途注入写入失败：events 插入被 ABORT，此时 request/task/attempt
    // 已写入、plan 尚未置 used，必须整体回滚。
    store
        .conn()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER wp1a_inject_fail BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();
    let err = store
        .submit(submit_input("req-f", "plan_f", T0))
        .unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "实际错误: {err:?}");

    assert_eq!(count(&store, "tasks"), 0);
    assert_eq!(count(&store, "attempts"), 0);
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "requests"), 0);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_f'"),
        "unused"
    );

    // 撤除注入后，同一请求重送成功，plan 正常消费。
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1a_inject_fail;")
        .unwrap();
    let outcome = store.submit(submit_input("req-f", "plan_f", T0)).unwrap();
    assert!(!outcome.deduplicated);
    assert_eq!(count(&store, "tasks"), 1);
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "requests"), 1);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_f'"),
        "used"
    );

    remove_dir(&dir);
}

#[test]
fn wal_and_durability_configuration() {
    let dir = temp_dir("pragmas");
    let store = Store::open(&dir.join("relay.db")).unwrap();

    let journal: String = store
        .conn()
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    let synchronous: i64 = store
        .conn()
        .unwrap()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(synchronous, 2, "synchronous 应为 FULL(2)");
    let foreign_keys: i64 = store
        .conn()
        .unwrap()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(foreign_keys, 1);
    let busy: i64 = store
        .conn()
        .unwrap()
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(busy, 5000);
    // schema v2 应包含启动栅栏表。
    assert_eq!(
        sqlite_master_count(&store.conn().unwrap(), "launch_fences"),
        1
    );

    let version = sqlite_version();
    assert_eq!(
        version.split('.').count(),
        3,
        "SQLite 版本应为 x.y.z: {version}"
    );
    eprintln!("bundled SQLite version: {version}");

    remove_dir(&dir);
}

#[test]
fn newer_schema_version_is_rejected_on_open() {
    let dir = temp_dir("schema-too-new");
    let db = dir.join("relay.db");
    {
        Store::open(&db).unwrap();
    }
    {
        // fixture 调整为未知更高版本 v10（本构建支持 v9）。
        let raw = Connection::open(&db).unwrap();
        raw.execute_batch("UPDATE meta SET value = '10' WHERE key = 'schema_version';")
            .unwrap();
    }
    let err = match Store::open(&db) {
        Err(err) => err,
        Ok(_) => panic!("schema v10 应被拒绝打开"),
    };
    assert!(
        matches!(
            err,
            StoreError::SchemaTooNew {
                found: 10,
                supported: 9
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.code(), "UNSUPPORTED_VERSION");

    remove_dir(&dir);
}

#[test]
fn unknown_ids_return_none_and_unknown_plan_is_invalid() {
    let dir = temp_dir("unknown-ids");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    assert!(store.get_task("task_nope").unwrap().is_none());
    assert!(store.get_request_result("req_nope").unwrap().is_none());

    let err = store
        .submit(submit_input("req-unknown", "plan_missing", T0))
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    assert_eq!(count(&store, "tasks"), 0);
    assert_eq!(count(&store, "requests"), 0);

    remove_dir(&dir);
}

#[test]
fn expiry_uses_instant_comparison_mixed_precision() {
    let dir = temp_dir("expiry-instant");
    let store = Store::open(&dir.join("relay.db")).unwrap();

    // 原缺陷回归：整秒 now（01:00:00Z）早于小数秒 expires_at
    // （01:00:00.500Z）。字符串比较把 '.'(0x2E) 排在 'Z'(0x5A) 之前，
    // 会误报 PLAN_EXPIRED；按时间点比较必须成功。
    store
        .put_plan(plan_input("plan_m1", T0, "2026-11-22T01:00:00.500Z"))
        .unwrap();
    let ok = store
        .submit(submit_input("req-m1", "plan_m1", "2026-11-22T01:00:00Z"))
        .unwrap();
    assert!(!ok.deduplicated);

    // 等于到期点：01:00:00Z 与 01:00:00.000Z 是同一时刻 → 过期。
    store
        .put_plan(plan_input("plan_m2", T0, "2026-11-22T01:00:00Z"))
        .unwrap();
    let err = store
        .submit(submit_input(
            "req-m2",
            "plan_m2",
            "2026-11-22T01:00:00.000Z",
        ))
        .unwrap_err();
    assert_eq!(err.code(), "PLAN_EXPIRED");
    // 到期点之前仍有效。
    let ok_in_window = store
        .submit(submit_input(
            "req-m2b",
            "plan_m2",
            "2026-11-22T00:59:59.999Z",
        ))
        .unwrap();
    assert!(!ok_in_window.deduplicated);

    // 小数秒 now 晚于整秒 expires_at → 过期。
    store
        .put_plan(plan_input("plan_m3", T0, "2026-11-22T01:00:00Z"))
        .unwrap();
    let err = store
        .submit(submit_input(
            "req-m3",
            "plan_m3",
            "2026-11-22T01:00:00.001Z",
        ))
        .unwrap_err();
    assert_eq!(err.code(), "PLAN_EXPIRED");

    // 去重仍先于到期检查：req-m1 在 plan_m1 已消费且已过期后重放，
    // 仍返回原结果。
    let replay = store
        .submit(submit_input("req-m1", "plan_m1", T60))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.task_id, ok.task_id);

    // 无效日期被拒绝：put_plan 与 submit 两侧。
    assert!(matches!(
        store.put_plan(plan_input("plan_bad", T0, "2026-02-30T00:00:00Z")),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.put_plan(plan_input("plan_bad", T0, "2026-02-29T00:00:00Z")),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.put_plan(plan_input("plan_bad", T0, "2026-11-22T01:00:00+08:00")),
        Err(StoreError::InvalidInput(_))
    ));
    assert!(matches!(
        store.put_plan(plan_input("plan_bad", "2026-11-22 01:00:00Z", T15)),
        Err(StoreError::InvalidInput(_))
    ));
    store.put_plan(plan_input("plan_m4", T0, T15)).unwrap();
    let err = store
        .submit(submit_input("req-m4", "plan_m4", "2026-11-22T01:00:00Z "))
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");

    // 只有两次成功提交建了行；失败路径不留行。
    assert_eq!(count(&store, "tasks"), 2);
    assert_eq!(count(&store, "requests"), 2);

    remove_dir(&dir);
}

#[test]
fn unsupported_schema_rejected_before_persistent_changes() {
    let dir = temp_dir("schema-no-write");
    let db = dir.join("relay.db");
    {
        Store::open(&db).unwrap();
    }
    {
        let raw = Connection::open(&db).unwrap();
        // fixture 调整为未知更高版本 v10（本构建支持 v9）。
        raw.execute_batch("UPDATE meta SET value = '10' WHERE key = 'schema_version';")
            .unwrap();
        // 模拟旧式非 WAL 数据库。
        let _: String = raw
            .query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))
            .unwrap();
    }
    let probe = Connection::open(&db).unwrap();
    let mode_before: String = probe
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode_before, "delete");
    drop(probe);
    let before = fs::read(&db).unwrap();

    let err = match Store::open(&db) {
        Err(err) => err,
        Ok(_) => panic!("schema v10 应被拒绝打开"),
    };
    assert_eq!(err.code(), "UNSUPPORTED_VERSION");

    // 拒绝发生在 journal_mode 切换与建表之前：文件字节未变、
    // journal_mode 仍为 delete、meta 仍为 3、无 WAL 侧车文件。
    let after = fs::read(&db).unwrap();
    assert_eq!(before, after, "失败的打开不得修改数据库文件");
    let probe = Connection::open(&db).unwrap();
    let mode_after: String = probe
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode_after, "delete");
    let version: String = probe
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "10");
    drop(probe);
    assert!(!dir.join("relay.db-wal").exists(), "不应创建 WAL 侧车文件");

    remove_dir(&dir);
}

#[test]
fn replay_returns_first_submit_snapshot_not_live_state() {
    let dir = temp_dir("replay-snapshot");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_s", T0, T15)).unwrap();
        store.submit(submit_input("req-s", "plan_s", T0)).unwrap()
    };
    {
        // 模拟后续任务包推进状态：queued/revision 1 → preparing/revision 2。
        let store = Store::open(&db).unwrap();
        let task = &first.task_id;
        store
            .conn()
            .unwrap()
            .execute_batch(&format!(
                "UPDATE tasks SET state = 'preparing', revision = 2, updated_at = '{T15}'
                  WHERE task_id = '{task}';
                 UPDATE attempts SET state = 'preparing', revision = 2
                  WHERE task_id = '{task}' AND attempt_no = 1;
                 INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
                  VALUES ('{task}', 1, 2, 'claimed', 'preparing', NULL, '{T15}');"
            ))
            .unwrap();
    }
    // 数据库重开后重放：即使 now 已过期，去重优先，返回首次提交快照。
    let store = Store::open(&db).unwrap();
    let replay = store.submit(submit_input("req-s", "plan_s", T60)).unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.task_id, first.task_id);
    assert_eq!(replay.result_json, first.result_json);
    assert_eq!(replay.created_at, T0);
    // 结构化字段与 result_json 同源：仍是首次提交的 queued/1/1，
    // 而不是 tasks 表推进后的 preparing/2。
    assert_eq!(replay.state, State::Queued);
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.active_attempt, 1);
    // 实时状态仍由 get_task 提供。
    let live = store.get_task(&first.task_id).unwrap().unwrap();
    assert_eq!(live.state, State::Preparing);
    assert_eq!(live.revision, 2);
    assert_eq!(live.updated_at, T15);

    remove_dir(&dir);
}

#[test]
fn expiry_compares_fraction_digits_precisely() {
    let dir = temp_dir("frac-precision");
    let store = Store::open(&dir.join("relay.db")).unwrap();

    // 截断实现的缺陷用例：expires_at 与 now 只差第 10 位小数（…891 vs …890），
    // 截断到纳秒后误报 PLAN_EXPIRED；精确比较后提交必须成功。
    store
        .put_plan(plan_input("plan_p1", T0, "2026-11-22T01:00:00.1234567891Z"))
        .unwrap();
    let ok = store
        .submit(submit_input(
            "req-p1",
            "plan_p1",
            "2026-11-22T01:00:00.1234567890Z",
        ))
        .unwrap();
    assert!(!ok.deduplicated);

    // 早于到期点可提交：.4999999999Z < .5Z。
    store
        .put_plan(plan_input("plan_p2", T0, "2026-11-22T01:00:00.5Z"))
        .unwrap();
    let ok_in_window = store
        .submit(submit_input(
            "req-p2",
            "plan_p2",
            "2026-11-22T01:00:00.4999999999Z",
        ))
        .unwrap();
    assert!(!ok_in_window.deduplicated);

    // 等于到期点：.5000000000Z 与 .5Z 同一刻度 → PLAN_EXPIRED。
    store
        .put_plan(plan_input("plan_p3", T0, "2026-11-22T01:00:00.5Z"))
        .unwrap();
    let err = store
        .submit(submit_input(
            "req-p3",
            "plan_p3",
            "2026-11-22T01:00:00.5000000000Z",
        ))
        .unwrap_err();
    assert_eq!(err.code(), "PLAN_EXPIRED");

    // 尾零不改变时间点：.100Z 与 .1Z 同一刻度，等于到期点 → PLAN_EXPIRED。
    store
        .put_plan(plan_input("plan_p4", T0, "2026-11-22T01:00:00.1Z"))
        .unwrap();
    let err = store
        .submit(submit_input(
            "req-p4",
            "plan_p4",
            "2026-11-22T01:00:00.100Z",
        ))
        .unwrap_err();
    assert_eq!(err.code(), "PLAN_EXPIRED");

    // 只有早于到期点的两次提交建了行。
    assert_eq!(count(&store, "tasks"), 2);
    assert_eq!(count(&store, "requests"), 2);

    remove_dir(&dir);
}

#[test]
fn leap_second_is_rejected_without_writes() {
    let dir = temp_dir("leap-second");
    let store = Store::open(&dir.join("relay.db")).unwrap();

    // 秒值 60 不在接受子集内：put_plan 侧拒绝（不建 plan 行）。
    assert!(matches!(
        store.put_plan(plan_input("plan_q1", T0, "2016-12-31T23:59:60Z")),
        Err(StoreError::InvalidInput(_))
    ));
    // submit 侧拒绝：now 含秒值 60 → INVALID_ARGUMENT。
    store.put_plan(plan_input("plan_q2", T0, T15)).unwrap();
    let err = store
        .submit(submit_input("req-q2", "plan_q2", "2026-11-22T01:00:60Z"))
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");

    // 非法输入不产生写入：无任务/尝试/事件/请求，plan_q2 未被消费。
    assert_eq!(count(&store, "tasks"), 0);
    assert_eq!(count(&store, "attempts"), 0);
    assert_eq!(count(&store, "events"), 0);
    assert_eq!(count(&store, "requests"), 0);
    assert_eq!(
        scalar(&store, "SELECT status FROM plans WHERE plan_id = 'plan_q2'"),
        "unused"
    );

    remove_dir(&dir);
}

/// attempt 1 行的 (state, revision)。
fn attempt_row(store: &Store, task_id: &str) -> (String, i64) {
    store
        .conn()
        .unwrap()
        .query_row(
            "SELECT state, revision FROM attempts WHERE task_id = ?1 AND attempt_no = 1",
            rusqlite::params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[test]
fn claim_persists_across_reopen_and_replay_keeps_snapshot() {
    let dir = temp_dir("cas-claim");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_t1", T0, T15)).unwrap();
        store.submit(submit_input("req-t1", "plan_t1", T0)).unwrap()
    };
    let outcome = {
        let store = Store::open(&db).unwrap();
        store
            .transition(TransitionInput {
                task_id: &first.task_id,
                expected_active_attempt: 1,
                expected_revision: 1,
                event: Event::Claim,
                now: T15,
            })
            .unwrap()
    };
    assert_eq!(
        outcome,
        TransitionOutcome {
            task_id: first.task_id.clone(),
            state: State::Preparing,
            revision: 2,
            active_attempt: 1,
            event_seq: 2,
        }
    );

    // 重开库核对 task、attempt 与事件。
    let store = Store::open(&db).unwrap();
    let task = store.get_task(&first.task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(task.active_attempt, 1);
    assert_eq!(task.updated_at, T15);
    assert_eq!(attempt_row(&store, &first.task_id), ("preparing".into(), 2));
    let events: Vec<(i64, String, i64, i64, String, String)> = store
        .conn()
        .unwrap()
        .prepare(
            "SELECT seq, kind, revision, attempt_no, state, created_at FROM events ORDER BY seq",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        (
            events[0].0,
            events[0].1.as_str(),
            events[0].2,
            events[0].4.as_str(),
            events[0].5.as_str()
        ),
        (1, "submitted", 1, "queued", T0)
    );
    assert_eq!(
        (
            events[1].0,
            events[1].1.as_str(),
            events[1].2,
            events[1].4.as_str(),
            events[1].5.as_str()
        ),
        (2, "claim", 2, "preparing", T15)
    );
    assert_eq!(events[1].3, 1);

    // 重放原 submit（此时 plan 已过期）：仍返回首次提交的 queued/revision 1
    // 快照；实时状态只经 get_task。
    let replay = store
        .submit(submit_input("req-t1", "plan_t1", T60))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.state, State::Queued);
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.active_attempt, 1);
    assert_eq!(replay.result_json, first.result_json);
    assert_eq!(
        store.get_task(&first.task_id).unwrap().unwrap().state,
        State::Preparing
    );

    remove_dir(&dir);
}

#[test]
fn concurrent_claims_one_winner_one_stale_revision() {
    let dir = temp_dir("cas-race");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_t2", T0, T15)).unwrap();
        store.submit(submit_input("req-t2", "plan_t2", T0)).unwrap()
    };
    let results: Vec<Result<TransitionOutcome, StoreError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let db = &db;
                let task_id = first.task_id.clone();
                scope.spawn(move || {
                    let store = Store::open(db).unwrap();
                    store.transition(TransitionInput {
                        task_id: &task_id,
                        expected_active_attempt: 1,
                        expected_revision: 1,
                        event: Event::Claim,
                        now: T15,
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut ok_count = 0;
    let mut stale_count = 0;
    let mut winner_task = None;
    for result in results {
        match result {
            Ok(outcome) => {
                ok_count += 1;
                assert_eq!(outcome.state, State::Preparing);
                assert_eq!(outcome.revision, 2);
                winner_task = Some(outcome.task_id);
            }
            Err(StoreError::StaleRevision {
                current_revision,
                current_active_attempt,
                ..
            }) => {
                stale_count += 1;
                assert_eq!((current_revision, current_active_attempt), (2, 1));
            }
            Err(err) => panic!("意外的错误变体: {err:?}"),
        }
    }
    assert_eq!((ok_count, stale_count), (1, 1));

    // revision 只增加一次，事件只追加一条。
    let store = Store::open(&db).unwrap();
    let task = store
        .get_task(winner_task.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(attempt_row(&store, &first.task_id), ("preparing".into(), 2));
    assert_eq!(count(&store, "events"), 2);

    remove_dir(&dir);
}

#[test]
fn transition_rejections_leave_no_writes() {
    let dir = temp_dir("cas-reject");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    store.put_plan(plan_input("plan_t3", T0, T15)).unwrap();
    let first = store.submit(submit_input("req-t3", "plan_t3", T0)).unwrap();

    // 过期 revision（预期 2，实际 1）。
    let err = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 2,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::StaleRevision {
            current_revision: 1,
            current_active_attempt: 1,
            ..
        }
    ));
    assert_eq!(err.code(), "STALE_REVISION");

    // 错误 attempt（预期 2，实际 1）。
    let err = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 2,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::StaleRevision {
            current_active_attempt: 1,
            ..
        }
    ));
    assert_eq!(err.code(), "STALE_REVISION");

    // 缺失 task。
    let err = store
        .transition(TransitionInput {
            task_id: "task_nope",
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }));
    assert_eq!(err.code(), "NOT_FOUND");

    // 非法转换：queued 不接受 Spawned。
    let err = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Spawned,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::WrongState {
                state: State::Queued,
                event: Event::Spawned,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.code(), "WRONG_STATE");

    // 全部拒绝后零写入：task/attempt 保持 queued/1，事件仍只有 1 条。
    let task = store.get_task(&first.task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, 1);
    assert_eq!(task.updated_at, T0);
    assert_eq!(attempt_row(&store, &first.task_id), ("queued".into(), 1));
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "attempts"), 1);
    assert_eq!(count(&store, "requests"), 1);

    remove_dir(&dir);
}

#[test]
fn injected_event_failure_rolls_back_transition() {
    let dir = temp_dir("cas-fault");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_t4", T0, T15)).unwrap();
        store.submit(submit_input("req-t4", "plan_t4", T0)).unwrap()
    };
    let store = Store::open(&db).unwrap();
    store
        .conn()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER wp1b_inject_fail BEFORE INSERT ON events
             BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
        )
        .unwrap();
    let err = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "实际错误: {err:?}");

    // task 与 attempt 同时回滚，无部分更新。
    let task = store.get_task(&first.task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, 1);
    assert_eq!(attempt_row(&store, &first.task_id), ("queued".into(), 1));
    assert_eq!(count(&store, "events"), 1);

    // 撤销注入后，原 CAS 可以成功。
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1b_inject_fail;")
        .unwrap();
    let outcome = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome.state, State::Preparing);
    assert_eq!(outcome.revision, 2);
    assert_eq!(count(&store, "events"), 2);

    remove_dir(&dir);
}

#[test]
fn injected_event_ignore_rejects_transition_without_writes() {
    let dir = temp_dir("cas-ignore");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_t5", T0, T15)).unwrap();
        store.submit(submit_input("req-t5", "plan_t5", T0)).unwrap()
    };
    let store = Store::open(&db).unwrap();
    // RAISE(IGNORE) 静默跳过事件插入：必须确认恰好写入一行才能提交。
    store
        .conn()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER wp1b_ignore_events BEFORE INSERT ON events
             BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    let err = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Environment(_)),
        "实际错误: {err:?}"
    );

    // 无事件写入时状态变更不得提交：task/attempt/updated_at/事件保持原状。
    let task = store.get_task(&first.task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, 1);
    assert_eq!(task.updated_at, T0);
    assert_eq!(attempt_row(&store, &first.task_id), ("queued".into(), 1));
    assert_eq!(count(&store, "events"), 1);

    // 撤销注入后，同一 CAS 成功并返回新事件的 seq。
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1b_ignore_events;")
        .unwrap();
    let outcome = store
        .transition(TransitionInput {
            task_id: &first.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome.state, State::Preparing);
    assert_eq!(outcome.revision, 2);
    assert_eq!(outcome.event_seq, 2);
    assert_eq!(count(&store, "events"), 2);

    remove_dir(&dir);
}

#[test]
fn revision_overflow_returns_explicit_error_without_writes() {
    let dir = temp_dir("cas-overflow");
    let db = dir.join("relay.db");
    let first = {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input("plan_t6", T0, T15)).unwrap();
        store.submit(submit_input("req-t6", "plan_t6", T0)).unwrap()
    };
    let store = Store::open(&db).unwrap();
    let task_id = &first.task_id;
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET revision = 9223372036854775807 WHERE task_id = '{task_id}';
             UPDATE attempts SET revision = 9223372036854775807
              WHERE task_id = '{task_id}' AND attempt_no = 1;"
        ))
        .unwrap();

    // revision 已达 i64::MAX：合法转换返回明确存储错误，无 panic、无写入。
    let err = store
        .transition(TransitionInput {
            task_id,
            expected_active_attempt: 1,
            expected_revision: i64::MAX as u64,
            event: Event::Claim,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Environment(_)),
        "实际错误: {err:?}"
    );
    let task = store.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, i64::MAX as u64);
    assert_eq!(task.updated_at, T0);
    assert_eq!(attempt_row(&store, task_id), ("queued".into(), i64::MAX));
    assert_eq!(count(&store, "events"), 1);
    assert_eq!(count(&store, "requests"), 1);

    // 同一 Store 仍可正常读取。
    assert!(store.get_request_result("req-t6").unwrap().is_some());

    remove_dir(&dir);
}

/// 某任务的事件条数。
fn count_events(store: &Store, task_id: &str) -> i64 {
    store
        .conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1",
            rusqlite::params![task_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// 恢复场景共用的任务搬运：提交后把 task 与 attempt 1 搬到指定状态。
fn stage_task(store: &Store, index: usize, state: &str) -> (String, String, String) {
    let plan_id = format!("plan_r{index}");
    let request_id = format!("req_r{index}");
    store.put_plan(plan_input(&plan_id, T0, T15)).unwrap();
    let outcome = store
        .submit(submit_input(&request_id, &plan_id, T0))
        .unwrap();
    let task_id = &outcome.task_id;
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = '{state}' WHERE task_id = '{task_id}';
             UPDATE attempts SET state = '{state}' WHERE task_id = '{task_id}' AND attempt_no = 1;"
        ))
        .unwrap();
    (outcome.task_id, request_id, plan_id)
}

#[test]
fn recover_maps_active_states_and_is_idempotent() {
    // 13 个状态各一个任务：5 个活动状态应被归入 interrupted，
    // 其余 8 个保持原样（不改 revision 或时间，不写事件）。
    let all_states: [&str; 13] = [
        "queued",
        "preparing",
        "launching",
        "running",
        "verifying",
        "cancelling",
        "awaiting_review",
        "blocked",
        "failed",
        "cancelled",
        "interrupted",
        "rejected",
        "completed",
    ];
    let changed_states: [&str; 5] = [
        "preparing",
        "launching",
        "running",
        "verifying",
        "cancelling",
    ];
    let dir = temp_dir("b2-recover");
    let db = dir.join("relay.db");
    let mut staged: Vec<(&str, String)> = Vec::new();
    let mut submitted: Vec<(&str, String, String, String)> = Vec::new(); // (state, request, plan, result_json)
    {
        let store = Store::open(&db).unwrap();
        for (i, state) in all_states.iter().enumerate() {
            let (task_id, request_id, plan_id) = stage_task(&store, i, state);
            staged.push((*state, task_id.clone()));
            submitted.push((*state, request_id, plan_id, outcome_json(&store, &task_id)));
        }
    }

    // 重开库后显式恢复。
    let store = Store::open(&db).unwrap();
    let records: Vec<RecoveryRecord> = store.recover(T60).unwrap();
    assert_eq!(records.len(), 5);
    let expected_ids: HashSet<String> = staged
        .iter()
        .filter(|(state, _)| changed_states.contains(state))
        .map(|(_, task_id)| task_id.clone())
        .collect();
    let actual_ids: HashSet<String> = records.iter().map(|r| r.task_id.clone()).collect();
    assert_eq!(actual_ids, expected_ids);
    for record in &records {
        assert_eq!(record.state, State::Interrupted);
        assert_eq!(record.revision, 2);
        assert_eq!(record.active_attempt, 1);
    }

    // 逐表核对：变更任务 vs 保持任务。
    for (state, task_id) in &staged {
        let task = store.get_task(task_id).unwrap().unwrap();
        if changed_states.contains(state) {
            assert_eq!(task.state, State::Interrupted);
            assert_eq!(task.revision, 2);
            assert_eq!(task.updated_at, T60);
            assert_eq!(attempt_row(&store, task_id), ("interrupted".into(), 2));
            assert_eq!(count_events(&store, task_id), 2);
            let (kind, rev, ev_state, attempt_no, created): (String, i64, String, i64, String) =
                store
                    .conn()
                    .unwrap()
                    .query_row(
                        "SELECT kind, revision, state, attempt_no, created_at
                         FROM events WHERE task_id = ?1 AND revision = 2",
                        rusqlite::params![task_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .unwrap();
            assert_eq!(
                (
                    kind.as_str(),
                    rev,
                    ev_state.as_str(),
                    attempt_no,
                    created.as_str()
                ),
                ("mark_interrupted", 2, "interrupted", 1, T60)
            );
        } else {
            assert_eq!(
                scalar(
                    &store,
                    &format!("SELECT state FROM tasks WHERE task_id = '{task_id}'")
                ),
                *state
            );
            assert_eq!(task.revision, 1);
            assert_eq!(task.updated_at, T0);
            assert_eq!(attempt_row(&store, task_id), (state.to_string(), 1));
            assert_eq!(count_events(&store, task_id), 1);
        }
    }
    assert_eq!(count(&store, "events"), 18);

    // 原 submit 快照不受恢复影响（以 running 任务为例）。
    let (_, request_id, plan_id, result_json) = submitted
        .iter()
        .find(|(state, _, _, _)| *state == "running")
        .unwrap();
    let replay = store
        .submit(submit_input(request_id, plan_id, T60))
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.state, State::Queued);
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.result_json, result_json.as_str());

    // 再次调用：无活动状态剩余，不追加事件、不改 revision 或时间。
    let records_again = store.recover(T60).unwrap();
    assert!(records_again.is_empty());
    assert_eq!(count(&store, "events"), 18);
    for (state, task_id) in &staged {
        let task = store.get_task(task_id).unwrap().unwrap();
        if changed_states.contains(state) {
            assert_eq!(task.revision, 2);
            assert_eq!(task.updated_at, T60);
        }
    }

    remove_dir(&dir);
}

/// 读取任务当前 result_json（供快照核对）。
fn outcome_json(store: &Store, task_id: &str) -> String {
    store
        .conn()
        .unwrap()
        .query_row(
            "SELECT result_json FROM requests WHERE task_id = ?1 LIMIT 1",
            rusqlite::params![task_id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn recover_batch_rolls_back_when_second_event_write_fails() {
    let dir = temp_dir("b2-rollback");
    let db = dir.join("relay.db");
    let mut task_ids: Vec<String> = Vec::new();
    {
        let store = Store::open(&db).unwrap();
        for i in 0..2 {
            let (task_id, _, _) = stage_task(&store, i, "preparing");
            task_ids.push(task_id);
        }
    }
    let store = Store::open(&db).unwrap();
    // 按创建顺序第二条恢复事件的写入失败。
    let second = &task_ids[1];
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER wp1b2_fail_second BEFORE INSERT ON events
             WHEN new.task_id = '{second}' AND new.kind = 'mark_interrupted'
             BEGIN SELECT RAISE(ABORT, 'injected second recovery failure'); END;"
        ))
        .unwrap();
    let err = store.recover(T60).unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "实际错误: {err:?}");

    // 整批回滚：第一条也已回滚，两个任务保持 preparing/1。
    for task_id in &task_ids {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Preparing);
        assert_eq!(task.revision, 1);
        assert_eq!(task.updated_at, T0);
        assert_eq!(attempt_row(&store, task_id), ("preparing".into(), 1));
        assert_eq!(count_events(&store, task_id), 1);
    }
    assert_eq!(count(&store, "events"), 2);

    // 撤销注入后整批可恢复。
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1b2_fail_second;")
        .unwrap();
    let records = store.recover(T60).unwrap();
    assert_eq!(records.len(), 2);
    for (record, task_id) in records.iter().zip(&task_ids) {
        assert_eq!(record.task_id, *task_id);
        assert_eq!(record.state, State::Interrupted);
        assert_eq!(record.revision, 2);
    }
    assert_eq!(count(&store, "events"), 4);

    remove_dir(&dir);
}

#[test]
fn recover_rejects_inconsistent_pairs_rolling_back_batch() {
    let dir = temp_dir("b2-inconsistent");
    let db = dir.join("relay.db");
    let mut task_ids: Vec<String> = Vec::new();
    {
        let store = Store::open(&db).unwrap();
        for i in 0..3 {
            let (task_id, _, _) = stage_task(&store, i, "preparing");
            task_ids.push(task_id);
        }
    }
    let store = Store::open(&db).unwrap();
    let (a, b, c) = (
        task_ids[0].clone(),
        task_ids[1].clone(),
        task_ids[2].clone(),
    );

    // 场景 1：task 与当前 attempt 状态不一致。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE attempts SET state = 'running' WHERE task_id = '{a}' AND attempt_no = 1;"
        ))
        .unwrap();
    let err = store.recover(T60).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    assert_batch_untouched(&store, &task_ids);

    // 还原 A，场景 2：当前 attempt 行缺失。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE attempts SET state = 'preparing' WHERE task_id = '{a}' AND attempt_no = 1;
             DELETE FROM attempts WHERE task_id = '{b}' AND attempt_no = 1;"
        ))
        .unwrap();
    let err = store.recover(T60).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    // B 的 attempt 行仍缺失（恢复不补行），其余原状。
    for task_id in [&a, &b, &c] {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Preparing);
        assert_eq!(task.revision, 1);
        assert_eq!(task.updated_at, T0);
        assert_eq!(count_events(&store, task_id), 1);
    }
    assert_eq!(count(&store, "events"), 3);

    // 还原 B，场景 3：未知状态文本。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
             VALUES ('{b}', 1, 'preparing', 1, NULL, NULL, '{T0}');
             UPDATE tasks SET state = 'bogus' WHERE task_id = '{c}';"
        ))
        .unwrap();
    let err = store.recover(T60).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    // C 的损坏状态未被触碰（未变成 interrupted），其余原状。
    let task = store.get_task(&a).unwrap().unwrap();
    assert_eq!((task.state, task.revision), (State::Preparing, 1));
    let task = store.get_task(&b).unwrap().unwrap();
    assert_eq!((task.state, task.revision), (State::Preparing, 1));
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT state FROM tasks WHERE task_id = '{c}'")
        ),
        "bogus"
    );
    assert_eq!(count(&store, "events"), 3);

    // 修正 C 后整批可恢复。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'preparing' WHERE task_id = '{c}';"
        ))
        .unwrap();
    let records = store.recover(T60).unwrap();
    assert_eq!(records.len(), 3);
    for record in &records {
        assert_eq!(record.state, State::Interrupted);
        assert_eq!(record.revision, 2);
    }
    assert_eq!(count(&store, "events"), 6);

    remove_dir(&dir);
}

/// 断言整批任务未被恢复触碰（损坏数据保持原样，不被静默修复）：
/// task 仍为 preparing/1、updated_at 不变、事件只有首条提交事件。
/// 不核对 attempt 行——场景中的损坏本身就留在那里。
fn assert_batch_untouched(store: &Store, task_ids: &[String]) {
    for task_id in task_ids {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Preparing);
        assert_eq!(task.revision, 1);
        assert_eq!(task.updated_at, T0);
        assert_eq!(count_events(store, task_id), 1);
    }
    assert_eq!(count(store, "events"), task_ids.len() as i64);
}

/// 入队一个 queued 任务（全局自增编号保证 ID 唯一），
/// 返回 (task_id, request_id, plan_id)。
fn enqueue_one(store: &Store, now: &str) -> (String, String, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let plan_id = format!("plan_q{index}");
    let request_id = format!("req_q{index}");
    store
        .put_plan(PlanInput {
            plan_id: &plan_id,
            project_id: "proj_f6a2",
            goal: "修复块计数在空输入时的返回值",
            brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
            base_oid: BASE_OID,
            created_at: T0,
            expires_at: TFAR,
            // Match the realistic plan used by retry tests: a retry must keep
            // the same scope, rule set and profile as the first submission.
            write_scope_json: "[\"src/count.rs\"]",
            rule_ids_json: "[\"cargo_test\"]",
            profile_id: "prof_mock",
        })
        .unwrap();
    let outcome = store
        .submit(submit_input(&request_id, &plan_id, now))
        .unwrap();
    (outcome.task_id, request_id, plan_id)
}

#[test]
fn count_nonterminal_tasks_excludes_terminal_states() {
    let dir = temp_dir("queue-count");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let (first, _, _) = enqueue_one(&store, T0);
    let (second, _, _) = enqueue_one(&store, T0);
    let (third, _, _) = enqueue_one(&store, T0);
    assert_eq!(store.count_nonterminal_tasks().unwrap(), 3);
    force_failed(&store, &first);
    assert_eq!(store.count_nonterminal_tasks().unwrap(), 2);
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state='completed' WHERE task_id='{second}';
             UPDATE attempts SET state='completed' WHERE task_id='{second}';"
        ))
        .unwrap();
    assert_eq!(store.count_nonterminal_tasks().unwrap(), 1);
    assert_eq!(
        store.get_task(&third).unwrap().unwrap().state,
        State::Queued
    );
    remove_dir(&dir);
}

#[test]
fn stage_run_projection_is_ordered_and_attempt_scoped() {
    let dir = temp_dir("stage-projection");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let (task, _, _) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &task,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    store
        .launch_fence(LaunchFenceInput {
            task_id: &task,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-stage",
            worktree_id: "wt-stage",
            now: T15,
        })
        .unwrap();
    store
        .begin_stage(
            relay_store::StageKey {
                task_id: &task,
                attempt_no: 1,
                round: 1,
                kind: relay_store::StageKind::Implement,
            },
            3,
            None,
            T15,
        )
        .unwrap();
    store
        .insert_managed_group(relay_store::ManagedGroupInput {
            task_id: &task,
            attempt_no: 1,
            pgid: 12345,
            leader_pid: 12345,
            leader_start: "stage-test",
            now: T15,
        })
        .unwrap();
    store.confirm_spawn_running(&task, 1, 3, T15).unwrap();
    store
        .finish_stage(
            relay_store::StageKey {
                task_id: &task,
                attempt_no: 1,
                round: 1,
                kind: relay_store::StageKind::Implement,
            },
            4,
            relay_store::StageFinish::Implemented,
            T15,
        )
        .unwrap();
    let rows = store.list_stage_runs(&task, 1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stage, "implement");
    assert_eq!(rows[0].state, "succeeded");
    assert!(store.list_stage_runs(&task, 2).unwrap().is_empty());
    remove_dir(&dir);
}

#[test]
fn claim_next_follows_persisted_order_and_skips_non_queued() {
    let dir = temp_dir("c1-order");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    // 提交时间逆序：q0 最晚时间提交（T60），q1 最早时间（T0）。
    let q0 = enqueue_one(&store, T60);
    let q1 = enqueue_one(&store, T0);
    let q2 = enqueue_one(&store, T15);
    let ids: Vec<String> = vec![q0.0.clone(), q1.0.clone(), q2.0.clone()];

    // 按持久化入队顺序领取，与提交时间无关。
    for expected in &ids {
        let outcome = store.claim_next(T15).unwrap().unwrap();
        assert_eq!(&outcome.task_id, expected);
        assert_eq!(outcome.state, State::Preparing);
        assert_eq!(outcome.revision, 2);
        assert_eq!(outcome.active_attempt, 1);
    }
    // 队列耗尽：None。
    assert!(store.claim_next(T15).unwrap().is_none());

    // 跳过非 queued：q3 置为 blocked，q4 仍 queued。
    let (q3, _, _) = enqueue_one(&store, T15);
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'blocked' WHERE task_id = '{q3}';
             UPDATE attempts SET state = 'blocked' WHERE task_id = '{q3}' AND attempt_no = 1;"
        ))
        .unwrap();
    let (q4, _, _) = enqueue_one(&store, T15);
    let outcome = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(outcome.task_id, q4);
    // blocked 任务未被触碰。
    let task = store.get_task(&q3).unwrap().unwrap();
    assert_eq!(task.state, State::Blocked);
    assert_eq!(task.revision, 1);
    assert_eq!(count_events(&store, &q3), 1);
    // 再领取：None。
    assert!(store.claim_next(T15).unwrap().is_none());
    // 事件总数：5 submitted + 4 claim。
    assert_eq!(count(&store, "events"), 9);

    remove_dir(&dir);
}

#[test]
fn concurrent_claims_serialize_without_sleep() {
    let dir = temp_dir("c1-race");
    let db = dir.join("relay.db");
    // 场景 A：仅一条候选，两个连接同时领取 → 恰好一次成功。
    {
        let store = Store::open(&db).unwrap();
        enqueue_one(&store, T0);
    }
    let results_a: Vec<Result<Option<TransitionOutcome>, StoreError>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let db = &db;
                    scope.spawn(move || {
                        let store = Store::open(db).unwrap();
                        store.claim_next(T15)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
    let mut some_count = 0;
    let mut none_count = 0;
    let mut winner = None;
    for result in results_a {
        match result.unwrap() {
            Some(outcome) => {
                some_count += 1;
                winner = Some(outcome.task_id);
            }
            None => none_count += 1,
        }
    }
    assert_eq!((some_count, none_count), (1, 1));
    let store = Store::open(&db).unwrap();
    let task = store.get_task(winner.as_deref().unwrap()).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(count(&store, "events"), 2);
    drop(store);

    // 场景 B：两条候选，两个连接同时领取 → 各领不同任务，每任务推进一次。
    {
        let store = Store::open(&db).unwrap();
        enqueue_one(&store, T0);
        enqueue_one(&store, T0);
    }
    let results_b: Vec<Result<Option<TransitionOutcome>, StoreError>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let db = &db;
                    scope.spawn(move || {
                        let store = Store::open(db).unwrap();
                        store.claim_next(T15)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
    let mut claimed: Vec<String> = Vec::new();
    for result in results_b {
        match result.unwrap() {
            Some(outcome) => {
                assert_eq!(outcome.state, State::Preparing);
                assert_eq!(outcome.revision, 2);
                claimed.push(outcome.task_id);
            }
            None => panic!("两条候选时应各领一条"),
        }
    }
    let unique: HashSet<&String> = claimed.iter().collect();
    assert_eq!(unique.len(), 2);
    // 每个任务只推进一次、只追加一条 claim 事件。
    // 事件总数：场景 A 2 条 + 场景 B 两条任务各 2 条 = 6。
    let store = Store::open(&db).unwrap();
    assert_eq!(count(&store, "events"), 6);
    for task_id in &claimed {
        assert_eq!(count_events(&store, task_id), 2);
    }

    remove_dir(&dir);
}

#[test]
fn claim_next_head_failure_rolls_back_without_skipping() {
    let dir = temp_dir("c1-rollback");
    let db = dir.join("relay.db");
    let mut ids: Vec<String> = Vec::new();
    {
        let store = Store::open(&db).unwrap();
        for _ in 0..2 {
            let (task_id, _, _) = enqueue_one(&store, T0);
            ids.push(task_id);
        }
    }
    let store = Store::open(&db).unwrap();
    let head = &ids[0];
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER wp1c1_fail_head BEFORE INSERT ON events
             WHEN new.task_id = '{head}' AND new.kind = 'claim'
             BEGIN SELECT RAISE(ABORT, 'injected head claim failure'); END;"
        ))
        .unwrap();
    let err = store.claim_next(T15).unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "实际错误: {err:?}");
    // 两条任务均不变。
    for task_id in &ids {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Queued);
        assert_eq!(task.revision, 1);
        assert_eq!(count_events(&store, task_id), 1);
    }
    assert_eq!(count(&store, "events"), 2);
    // 撤销注入后仍先领取原队首。
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1c1_fail_head;")
        .unwrap();
    let first = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(first.task_id, ids[0]);
    let second = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(second.task_id, ids[1]);
    assert_eq!(count(&store, "events"), 4);

    remove_dir(&dir);
}

#[test]
fn claim_next_rejects_inconsistent_head_without_skipping() {
    let dir = temp_dir("c1-inconsistent");
    let db = dir.join("relay.db");
    let mut ids: Vec<String> = Vec::new();
    {
        let store = Store::open(&db).unwrap();
        for _ in 0..2 {
            let (task_id, _, _) = enqueue_one(&store, T0);
            ids.push(task_id);
        }
    }
    let store = Store::open(&db).unwrap();
    let head = &ids[0];
    // 队首缺少 submitted 事件（数据不一致）：报错，不跳过队首。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "DELETE FROM events WHERE task_id = '{head}' AND kind = 'submitted';"
        ))
        .unwrap();
    let err = store.claim_next(T15).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    for task_id in &ids {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Queued);
        assert_eq!(task.revision, 1);
    }
    assert_eq!(count(&store, "events"), 1);
    // 还原队首 submitted 事件（保持原 seq），先领队首再领次条。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO events (seq, task_id, attempt_no, revision, kind, state, reason_code, created_at)
             VALUES (1, '{head}', 1, 1, 'submitted', 'queued', NULL, '{T0}');"
        ))
        .unwrap();
    let first = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(first.task_id, ids[0]);
    let second = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(second.task_id, ids[1]);

    remove_dir(&dir);
}

/// v1 fixture：与 WP1-A 建库一致的表结构（无 launch_fences），仅供迁移测试。
const V1_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS plans (
    plan_id    TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    goal       TEXT NOT NULL,
    brief      TEXT NOT NULL,
    base_oid   TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('unused', 'used'))
);

CREATE TABLE IF NOT EXISTS tasks (
    task_id        TEXT PRIMARY KEY,
    plan_id        TEXT NOT NULL REFERENCES plans(plan_id),
    project_id     TEXT NOT NULL,
    goal           TEXT NOT NULL,
    base_oid       TEXT NOT NULL,
    state          TEXT NOT NULL,
    revision       INTEGER NOT NULL CHECK (revision > 0),
    active_attempt INTEGER NOT NULL CHECK (active_attempt > 0),
    reason_code    TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_plan_id ON tasks(plan_id);

CREATE TABLE IF NOT EXISTS attempts (
    task_id    TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    state      TEXT NOT NULL,
    revision   INTEGER NOT NULL CHECK (revision > 0),
    started_at TEXT,
    ended_at   TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no  INTEGER NOT NULL CHECK (attempt_no > 0),
    revision    INTEGER NOT NULL CHECK (revision > 0),
    kind        TEXT NOT NULL,
    state       TEXT NOT NULL,
    reason_code TEXT,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS requests (
    request_id   TEXT PRIMARY KEY,
    operation    TEXT NOT NULL,
    params_canon TEXT NOT NULL,
    params_hash  TEXT NOT NULL,
    plan_id      TEXT NOT NULL REFERENCES plans(plan_id),
    task_id      TEXT NOT NULL REFERENCES tasks(task_id),
    result_json  TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_plan_id ON requests(plan_id);
";

/// 构造带历史数据（plan/task/attempt/两条事件/请求快照）的 v1 数据库。
fn build_v1_db(dir: &Path, task_state: &str) -> PathBuf {
    let db = dir.join("relay.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(V1_SCHEMA_SQL).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO meta(key, value) VALUES ('schema_version', '1');
         INSERT INTO plans (plan_id, project_id, goal, brief, base_oid, created_at, expires_at, status)
          VALUES ('plan_v1', 'proj_f6a2', 'v1 历史目标', 'v1 历史简报', '{BASE_OID}', '{T0}', '{TFAR}', 'used');
         INSERT INTO tasks (task_id, plan_id, project_id, goal, base_oid, state, revision, active_attempt, reason_code, created_at, updated_at)
          VALUES ('task_v1', 'plan_v1', 'proj_f6a2', 'v1 历史目标', '{BASE_OID}', '{task_state}', 2, 1, NULL, '{T0}', '{T15}');
         INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
          VALUES ('task_v1', 1, '{task_state}', 2, NULL, NULL, '{T0}');
         INSERT INTO events (seq, task_id, attempt_no, revision, kind, state, reason_code, created_at)
          VALUES (1, 'task_v1', 1, 1, 'submitted', 'queued', NULL, '{T0}');
         INSERT INTO events (seq, task_id, attempt_no, revision, kind, state, reason_code, created_at)
          VALUES (2, 'task_v1', 1, 2, 'claim', '{task_state}', NULL, '{T15}');
         INSERT INTO requests (request_id, operation, params_canon, params_hash, plan_id, task_id, result_json, created_at)
          VALUES ('req_v1', 'relay_submit', '{{\"plan_id\":\"plan_v1\"}}', '0123abcd', 'plan_v1', 'task_v1',
                  '{{\"schema_version\":1,\"task\":{{\"id\":\"task_v1\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}}}}', '{T0}');"
    ))
    .unwrap();
    drop(conn);
    db
}

fn table_count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn sqlite_master_count(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
        rusqlite::params![name],
        |r| r.get(0),
    )
    .unwrap()
}

fn meta_version(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn migrate_v1_to_v2_succeeds_and_backup_stays_v1() {
    let dir = temp_dir("c2a-migrate");
    let db = build_v1_db(&dir, "preparing");
    let backup = dir.join("backup-v1.db");
    migrate_v1_to_v2(&db, &backup).unwrap();
    // v2 仍需显式迁到 v3（artifacts）→ v4（handoff）→ v5 → v6（派发门）。
    let backup3 = dir.join("backup-v3.db");
    migrate_v2_to_v3(&db, &backup3).unwrap();
    migrate_v3_to_v4(&db, &dir.join("backup-v4.db")).unwrap();
    migrate_v4_to_v5(&db, &dir.join("backup-v5.db")).unwrap();
    migrate_v5_to_v6(&db, &dir.join("backup-v6.db")).unwrap();
    migrate_v6_to_v7(&db, &dir.join("backup-v7.db")).unwrap();
    migrate_v7_to_v8(&db, &dir.join("backup-v8.db")).unwrap();
    migrate_v8_to_v9(&db, &dir.join("backup-v9.db")).unwrap();

    // 源库已是 v6：版本号、launch_fences/artifacts/managed_groups 表、
    // 历史数据完好，Store::open 正常。
    let store = Store::open(&db).unwrap();
    assert_eq!(meta_version(&store.conn().unwrap()), "9");
    assert_eq!(sqlite_master_count(&store.conn().unwrap(), "artifacts"), 1);
    assert_eq!(
        sqlite_master_count(&store.conn().unwrap(), "managed_groups"),
        1
    );
    assert_eq!(
        sqlite_master_count(&store.conn().unwrap(), "launch_fences"),
        1
    );
    let task = store.get_task("task_v1").unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(count_events(&store, "task_v1"), 2);
    let req = store.get_request_result("req_v1").unwrap().unwrap();
    assert!(req.result_json.contains("task_v1"));
    drop(store);

    // 备份仍为 v1 且可读，历史行与源库迁移前一致。
    let backup_conn = Connection::open(&backup).unwrap();
    assert_eq!(meta_version(&backup_conn), "1");
    assert_eq!(table_count(&backup_conn, "tasks"), 1);
    assert_eq!(table_count(&backup_conn, "events"), 2);
    assert_eq!(table_count(&backup_conn, "requests"), 1);
    assert_eq!(sqlite_master_count(&backup_conn, "launch_fences"), 0);

    remove_dir(&dir);
}

#[test]
fn migrate_refuses_existing_backup_or_bad_target_without_touching_source() {
    let dir = temp_dir("c2a-backup");
    let db = build_v1_db(&dir, "preparing");
    let before = fs::read(&db).unwrap();

    // 已存在的备份不得覆盖。
    let backup = dir.join("backup-v1.db");
    fs::write(&backup, b"not a database").unwrap();
    let err = migrate_v1_to_v2(&db, &backup).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(err.to_string().contains("已存在"), "{err}");
    assert_eq!(fs::read(&db).unwrap(), before, "备份失败不得修改源库");
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "1");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 0);
    drop(probe);

    // 备份目标目录不存在：VACUUM INTO 失败，源库仍不被触碰。
    let bad_backup = dir.join("missing-dir").join("backup.db");
    let err = migrate_v1_to_v2(&db, &bad_backup).unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");
    assert_eq!(fs::read(&db).unwrap(), before, "备份失败不得修改源库");
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "1");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 0);

    remove_dir(&dir);
}

#[test]
fn migrate_failure_rolls_back_and_source_stays_v1() {
    let dir = temp_dir("c2a-rollback");
    let db = build_v1_db(&dir, "preparing");
    let backup = dir.join("backup-v1.db");
    {
        // 注入：迁移事务内的 meta 版本更新被 ABORT。
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER wp1c2a_fail_meta BEFORE UPDATE ON meta
             BEGIN SELECT RAISE(ABORT, 'injected meta update failure'); END;",
        )
        .unwrap();
    }
    let err = migrate_v1_to_v2(&db, &backup).unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");

    // 源库仍为 v1：launch_fences 建表随事务回滚，历史数据完好。
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "1");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 0);
    assert_eq!(table_count(&probe, "tasks"), 1);
    assert_eq!(table_count(&probe, "events"), 2);
    drop(probe);

    // 备份已产出且可读（仍为 v1）。
    let backup_conn = Connection::open(&backup).unwrap();
    assert_eq!(meta_version(&backup_conn), "1");
    assert_eq!(table_count(&backup_conn, "tasks"), 1);
    drop(backup_conn);

    // 撤销注入后重试成功（换新备份路径，不覆盖已有备份）。
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER wp1c2a_fail_meta;")
        .unwrap();
    drop(conn);
    let retry_backup = dir.join("backup-v1-retry.db");
    migrate_v1_to_v2(&db, &retry_backup).unwrap();
    migrate_v2_to_v3(&db, &dir.join("backup-v3-retry.db")).unwrap();
    migrate_v3_to_v4(&db, &dir.join("backup-v4-retry.db")).unwrap();
    migrate_v4_to_v5(&db, &dir.join("backup-v5-retry.db")).unwrap();
    migrate_v5_to_v6(&db, &dir.join("backup-v6-retry.db")).unwrap();
    migrate_v6_to_v7(&db, &dir.join("backup-v7-retry.db")).unwrap();
    migrate_v7_to_v8(&db, &dir.join("backup-v8-retry.db")).unwrap();
    migrate_v8_to_v9(&db, &dir.join("backup-v9-retry.db")).unwrap();
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "9");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 1);

    remove_dir(&dir);
}

#[test]
fn migrate_rejects_unknown_or_current_versions_without_writes() {
    let dir = temp_dir("c2a-versions");
    let db = build_v1_db(&dir, "preparing");
    let backup = dir.join("backup.db");
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("UPDATE meta SET value = '10' WHERE key = 'schema_version';")
            .unwrap();
    }
    let before = fs::read(&db).unwrap();
    let err = migrate_v1_to_v2(&db, &backup).unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::SchemaTooNew {
                found: 10,
                supported: 9
            }
        ),
        "{err:?}"
    );
    // 零持久写入：文件字节不变，未产出备份。
    assert_eq!(fs::read(&db).unwrap(), before);
    assert!(!backup.exists(), "版本核验失败不得产出备份");

    // 当前数据库：v8→v9 拒绝；v1→v2 报不是 v1。
    let v9_db = dir.join("relay-v9.db");
    {
        Store::open(&v9_db).unwrap();
    }
    let err = migrate_v8_to_v9(&v9_db, &dir.join("backup2.db")).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(err.to_string().contains("仅支持从 schema v8"));
    let err = migrate_v1_to_v2(&v9_db, &dir.join("backup3.db")).unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(err.to_string().contains("不是 v1"));

    remove_dir(&dir);
}

#[test]
fn open_v1_prompts_migration_without_persistent_write() {
    let dir = temp_dir("c2a-open-v1");
    let db = build_v1_db(&dir, "preparing");
    let before = fs::read(&db).unwrap();
    let err = match Store::open(&db) {
        Err(err) => err,
        Ok(_) => panic!("v1 库应被拒绝打开并提示迁移"),
    };
    assert!(
        matches!(
            err,
            StoreError::SchemaTooOld {
                found: 1,
                supported: 9
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.code(), "UNSUPPORTED_VERSION");
    assert!(
        err.to_string().contains("migrate_v1_to_v2"),
        "错误应提示迁移方法: {err}"
    );
    // 零持久写入：文件字节不变、无 WAL 侧车。
    assert_eq!(fs::read(&db).unwrap(), before, "open 不做静默升级");
    assert!(!dir.join("relay.db-wal").exists());

    // 迁移后可正常打开（v1→v2→v3→v4 链）。
    migrate_v1_to_v2(&db, &dir.join("backup-v1.db")).unwrap();
    migrate_v2_to_v3(&db, &dir.join("backup-v3.db")).unwrap();
    migrate_v3_to_v4(&db, &dir.join("backup-v4.db")).unwrap();
    migrate_v4_to_v5(&db, &dir.join("backup-v5.db")).unwrap();
    migrate_v5_to_v6(&db, &dir.join("backup-v6.db")).unwrap();
    migrate_v6_to_v7(&db, &dir.join("backup-v7.db")).unwrap();
    migrate_v7_to_v8(&db, &dir.join("backup-v8.db")).unwrap();
    migrate_v8_to_v9(&db, &dir.join("backup-v9.db")).unwrap();
    Store::open(&db).unwrap();

    remove_dir(&dir);
}

/// 领取一个任务到 preparing（rev 2），返回 task_id，供栅栏测试使用。
fn claim_one(store: &Store) -> String {
    let (task_id, _, _) = enqueue_one(store, T0);
    store
        .transition(TransitionInput {
            task_id: &task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    task_id
}

#[test]
fn launch_fence_persists_and_blocks_second_permit() {
    let dir = temp_dir("c2b-fence");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    let t = claim_one(&store);
    let permit = store
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-a",
            worktree_id: "wt-a",
            now: T15,
        })
        .unwrap();
    assert_eq!(
        permit,
        LaunchFenceOutcome {
            task_id: t.clone(),
            state: State::Launching,
            revision: 3,
            active_attempt: 1,
            launch_code: "code-a".into(),
            worktree_id: "wt-a".into(),
            event_seq: 3,
        }
    );

    // 持久化核对：task/attempt/事件/栅栏记录。
    let task = store.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Launching);
    assert_eq!(task.revision, 3);
    assert_eq!(task.updated_at, T15);
    assert_eq!(attempt_row(&store, &t), ("launching".into(), 3));
    assert_eq!(
        store.get_launch_fence(&t, 1).unwrap().unwrap(),
        LaunchFenceRecord {
            task_id: t.clone(),
            attempt_no: 1,
            launch_code: "code-a".into(),
            worktree_id: "wt-a".into(),
            created_at: T15.into(),
        }
    );
    assert_eq!(count_events(&store, &t), 3);
    let (kind, rev, ev_state): (String, i64, String) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT kind, revision, state FROM events WHERE task_id = ?1 AND revision = 3",
            rusqlite::params![t],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), rev, ev_state.as_str()),
        ("prepare_launch", 3, "launching")
    );

    // 同一 attempt 重送（旧 revision 预期）→ StaleRevision；换新代号亦然。
    for code in ["code-a", "code-a-new"] {
        let err = store
            .launch_fence(LaunchFenceInput {
                task_id: &t,
                expected_active_attempt: 1,
                expected_revision: 2,
                launch_code: code,
                worktree_id: "wt-a",
                now: T15,
            })
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::StaleRevision {
                    current_revision: 3,
                    current_active_attempt: 1,
                    ..
                }
            ),
            "{err:?}"
        );
    }
    // 重开库后查询仍可见，且不能获得第二次许可。
    let reopened = Store::open(&db).unwrap();
    assert!(reopened.get_launch_fence(&t, 1).unwrap().is_some());
    let err = reopened
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-reopen",
            worktree_id: "wt-reopen",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::StaleRevision { .. }), "{err:?}");
    assert_eq!(reopened.get_task(&t).unwrap().unwrap().revision, 3);
    // 栅栏记录仍是首次那一条。
    assert_eq!(
        reopened
            .get_launch_fence(&t, 1)
            .unwrap()
            .unwrap()
            .launch_code,
        "code-a"
    );

    // 缺失 task → NOT_FOUND。
    let err = reopened
        .launch_fence(LaunchFenceInput {
            task_id: "task_nope",
            expected_active_attempt: 1,
            expected_revision: 1,
            launch_code: "code-x",
            worktree_id: "wt-x",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");

    // 普通 transition 不能绕过：PrepareLaunch 被拒绝，状态不变、无栅栏。
    let t2 = claim_one(&reopened);
    let err = reopened
        .transition(TransitionInput {
            task_id: &t2,
            expected_active_attempt: 1,
            expected_revision: 2,
            event: Event::PrepareLaunch,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    assert!(err.to_string().contains("launch_fence"), "{err}");
    let task = reopened.get_task(&t2).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert!(reopened.get_launch_fence(&t2, 1).unwrap().is_none());
    assert_eq!(count_events(&store, &t2), 2);

    remove_dir(&dir);
}

#[test]
fn launch_fence_and_cancel_race_exactly_one_wins() {
    let dir = temp_dir("c2b-race");
    let db = dir.join("relay.db");
    {
        let store = Store::open(&db).unwrap();
        let t = claim_one(&store);
        assert_eq!(store.get_task(&t).unwrap().unwrap().revision, 2);
    }
    let task_id = {
        let store = Store::open(&db).unwrap();
        let rows: Vec<String> = store
            .conn()
            .unwrap()
            .prepare("SELECT task_id FROM tasks WHERE state = 'preparing'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.into_iter().next().unwrap()
    };
    // 同一旧 revision（2）上栅栏与取消竞争：恰好一次写入成功。
    let results: Vec<Result<(), StoreError>> = std::thread::scope(|scope| {
        let fence = {
            let db = &db;
            let task_id = task_id.clone();
            scope.spawn(move || {
                let store = Store::open(db).unwrap();
                store
                    .launch_fence(LaunchFenceInput {
                        task_id: &task_id,
                        expected_active_attempt: 1,
                        expected_revision: 2,
                        launch_code: "code-race",
                        worktree_id: "wt-race",
                        now: T15,
                    })
                    .map(|_| ())
            })
        };
        let cancel = {
            let db = &db;
            let task_id = task_id.clone();
            scope.spawn(move || {
                let store = Store::open(db).unwrap();
                store
                    .transition(TransitionInput {
                        task_id: &task_id,
                        expected_active_attempt: 1,
                        expected_revision: 2,
                        event: Event::CancelRequested,
                        now: T15,
                    })
                    .map(|_| ())
            })
        };
        vec![fence.join().unwrap(), cancel.join().unwrap()]
    });
    let mut fence_won = None;
    let mut ok_count = 0;
    for (which, result) in ["fence", "cancel"].iter().zip(results) {
        match result {
            Ok(()) => {
                ok_count += 1;
                fence_won = Some(*which == "fence");
            }
            Err(StoreError::StaleRevision { .. }) => {}
            Err(err) => panic!("意外的错误: {err:?}"),
        }
    }
    assert_eq!(ok_count, 1, "栅栏与取消只允许一次写入成功");
    let store = Store::open(&db).unwrap();
    match fence_won {
        Some(true) => {
            let task = store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(task.state, State::Launching);
            assert_eq!(task.revision, 3);
            assert!(store.get_launch_fence(&task_id, 1).unwrap().is_some());
        }
        Some(false) => {
            let task = store.get_task(&task_id).unwrap().unwrap();
            assert_eq!(task.state, State::Cancelled);
            assert_eq!(task.revision, 3);
            assert!(store.get_launch_fence(&task_id, 1).unwrap().is_none());
            assert_eq!(
                count(&store, "launch_fences"),
                0,
                "取消先提交时不发许可，栅栏不得落库"
            );
        }
        None => unreachable!(),
    }

    remove_dir(&dir);
}

#[test]
fn launch_fence_injections_roll_back_completely() {
    let dir = temp_dir("c2b-inject");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    let t = claim_one(&store);

    // ABORT 注入：事件写入失败，栅栏记录与状态一并回滚。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER wp1c2b_abort BEFORE INSERT ON events
             WHEN new.task_id = '{t}' AND new.kind = 'prepare_launch'
             BEGIN SELECT RAISE(ABORT, 'injected fence failure'); END;"
        ))
        .unwrap();
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-abort",
            worktree_id: "wt-abort",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");
    let task = store.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(attempt_row(&store, &t), ("preparing".into(), 2));
    assert!(store.get_launch_fence(&t, 1).unwrap().is_none());
    assert_eq!(count_events(&store, &t), 2);
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1c2b_abort;")
        .unwrap();

    // IGNORE 注入：事件被静默跳过，行数校验必须拒绝，整体回滚。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER wp1c2b_ignore BEFORE INSERT ON events
             WHEN new.task_id = '{t}' AND new.kind = 'prepare_launch'
             BEGIN SELECT RAISE(IGNORE); END;"
        ))
        .unwrap();
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-ignore",
            worktree_id: "wt-ignore",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    let task = store.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert!(store.get_launch_fence(&t, 1).unwrap().is_none());
    assert_eq!(count_events(&store, &t), 2);
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1c2b_ignore;")
        .unwrap();

    // 代号冲突：占用代号被拒绝，新任务不受影响。
    let permit = store
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-x",
            worktree_id: "wt-x",
            now: T15,
        })
        .unwrap();
    assert_eq!(permit.launch_code, "code-x");
    let other = claim_one(&store);
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &other,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-x",
            worktree_id: "wt-other",
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::LaunchCodeConflict { ref launch_code } if launch_code == "code-x"),
        "{err:?}"
    );
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let task = store.get_task(&other).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert!(store.get_launch_fence(&other, 1).unwrap().is_none());
    // 换新代号后成功。
    store
        .launch_fence(LaunchFenceInput {
            task_id: &other,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-y",
            worktree_id: "wt-y",
            now: T15,
        })
        .unwrap();
    assert_eq!(
        store
            .get_launch_fence(&other, 1)
            .unwrap()
            .unwrap()
            .launch_code,
        "code-y"
    );

    // 同一 attempt 重复栅栏（revision 被回拨的一致性损坏场景）：
    // 主键冲突报 Environment，事务回滚，原栅栏记录不变。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'preparing', revision = 2 WHERE task_id = '{t}';
             UPDATE attempts SET state = 'preparing', revision = 2 WHERE task_id = '{t}' AND attempt_no = 1;"
        ))
        .unwrap();
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-dup",
            worktree_id: "wt-dup",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    assert!(err.to_string().contains("已存在栅栏记录"), "{err}");
    let task = store.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert_eq!(
        store.get_launch_fence(&t, 1).unwrap().unwrap().launch_code,
        "code-x",
        "原栅栏记录不变"
    );

    remove_dir(&dir);
}

/// 模拟启动器：只记录 spawn 调用（task_id, attempt_no），通过调用计数与
/// 数据库状态联测栅栏语义；不是真实子进程验收。
#[derive(Default)]
struct FakeLauncher {
    spawn_calls: Vec<(String, u32)>,
}

impl FakeLauncher {
    fn spawn(&mut self, task_id: &str, attempt_no: u32) {
        self.spawn_calls.push((task_id.to_owned(), attempt_no));
    }

    fn count_for(&self, task_id: &str) -> usize {
        self.spawn_calls
            .iter()
            .filter(|(t, _)| t == task_id)
            .count()
    }
}

#[test]
fn simulated_launcher_matches_fence_and_recovery() {
    let dir = temp_dir("c2c-sim");
    let db = dir.join("relay.db");
    let mut launcher = FakeLauncher::default();
    let store = Store::open(&db).unwrap();

    // 失败路径 1：栅栏未提交（未调用专用方法）→ spawn 0 次。
    let t_no_fence = claim_one(&store);
    assert_eq!(launcher.spawn_calls.len(), 0);

    // 失败路径 2：事务失败（事件 ABORT 注入）→ spawn 0 次，状态与栅栏回滚。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER wp1c2c_abort BEFORE INSERT ON events
             WHEN new.task_id = '{t_no_fence}' AND new.kind = 'prepare_launch'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        ))
        .unwrap();
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t_no_fence,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "sim-a",
            worktree_id: "wt-sim-a",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");
    let task = store.get_task(&t_no_fence).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.revision, 2);
    assert!(store.get_launch_fence(&t_no_fence, 1).unwrap().is_none());
    assert_eq!(launcher.spawn_calls.len(), 0);
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1c2c_abort;")
        .unwrap();

    // 失败路径 3：取消先胜出 → 不发许可 → spawn 0 次。
    let t_cancel = claim_one(&store);
    store
        .transition(TransitionInput {
            task_id: &t_cancel,
            expected_active_attempt: 1,
            expected_revision: 2,
            event: Event::CancelRequested,
            now: T15,
        })
        .unwrap();
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t_cancel,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "sim-c",
            worktree_id: "wt-sim-c",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::StaleRevision { .. }), "{err:?}");
    assert_eq!(launcher.spawn_calls.len(), 0);
    assert!(store.get_launch_fence(&t_cancel, 1).unwrap().is_none());

    // 成功路径：commit 后、spawn 前即可从第二个数据库连接读到
    // launching 与栅栏记录。
    let t_ok = claim_one(&store);
    let permit = store
        .launch_fence(LaunchFenceInput {
            task_id: &t_ok,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "sim-ok",
            worktree_id: "wt-ok",
            now: T15,
        })
        .unwrap();
    assert_eq!(permit.state, State::Launching);
    assert_eq!(permit.revision, 3);
    let reader = Store::open(&db).unwrap();
    let task = reader.get_task(&t_ok).unwrap().unwrap();
    assert_eq!(task.state, State::Launching);
    assert_eq!(task.revision, 3);
    let fence = reader.get_launch_fence(&t_ok, 1).unwrap().unwrap();
    assert_eq!(
        (fence.launch_code.as_str(), fence.worktree_id.as_str()),
        ("sim-ok", "wt-ok")
    );
    // commit 成功才调用 spawn；同一 attempt 最多一次。
    launcher.spawn(&t_ok, 1);
    assert_eq!(launcher.count_for(&t_ok), 1);
    // 同一 attempt 重送拿不到第二次许可，spawn 不再被调用。
    let err = reader
        .launch_fence(LaunchFenceInput {
            task_id: &t_ok,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "sim-ok-2",
            worktree_id: "wt-ok-2",
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::StaleRevision { .. }), "{err:?}");
    assert_eq!(launcher.count_for(&t_ok), 1);

    // 栅栏已提交但尚未 spawn（重启场景 a 的主体）。
    let t_nospawn = claim_one(&store);
    store
        .launch_fence(LaunchFenceInput {
            task_id: &t_nospawn,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "sim-ns",
            worktree_id: "wt-ns",
            now: T15,
        })
        .unwrap();
    assert_eq!(launcher.count_for(&t_nospawn), 0);
    drop(store);
    drop(reader);

    // 模拟重启：重开库显式恢复。
    // - t_no_fence：preparing → interrupted（栅栏从未提交）；
    // - t_ok：launching、已 spawn 但未写回结果（重启场景 b）→ interrupted；
    // - t_nospawn：launching、栅栏已提交但未 spawn（重启场景 a）→ interrupted；
    // - t_cancel：cancelled 保持原样。
    let store = Store::open(&db).unwrap();
    let records = store.recover(T60).unwrap();
    assert_eq!(records.len(), 3);
    let recovered: Vec<&str> = records.iter().map(|r| r.task_id.as_str()).collect();
    assert!(recovered.contains(&t_no_fence.as_str()));
    assert!(recovered.contains(&t_ok.as_str()));
    assert!(recovered.contains(&t_nospawn.as_str()));
    assert!(!recovered.contains(&t_cancel.as_str()));
    for (task_id, expected_rev) in [(&t_no_fence, 3), (&t_ok, 4), (&t_nospawn, 4)] {
        let task = store.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, State::Interrupted);
        assert_eq!(task.revision, expected_rev);
    }
    // 两种重启都恢复为 interrupted，且不重发旧 attempt：spawn 调用数不变。
    assert_eq!(launcher.spawn_calls.len(), 1);
    assert_eq!(launcher.count_for(&t_ok), 1);
    assert_eq!(launcher.count_for(&t_nospawn), 0);
    assert_eq!(count_events(&store, &t_ok), 4);
    // 恢复保留栅栏记录（且不把它当作进程已启动或已退出的证据——记录
    // 内容与恢复前一致）。
    for t in [&t_ok, &t_nospawn] {
        let fence = store.get_launch_fence(t, 1).unwrap().unwrap();
        assert_eq!(fence.created_at, T15);
    }
    // 恢复后对旧 attempt 的栅栏请求仍被拒绝（无新许可、不再 spawn）。
    let err = store
        .launch_fence(LaunchFenceInput {
            task_id: &t_ok,
            expected_active_attempt: 1,
            expected_revision: 3,
            launch_code: "sim-re",
            worktree_id: "wt-re",
            now: T60,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::StaleRevision { .. }), "{err:?}");
    assert_eq!(launcher.count_for(&t_ok), 1);

    remove_dir(&dir);
}

#[test]
fn migrate_meta_update_ignore_is_detected_and_rolls_back() {
    let dir = temp_dir("c2a-ignore");
    let db = build_v1_db(&dir, "preparing");
    let backup = dir.join("backup-v1.db");
    {
        // 注入：meta 版本更新被 RAISE(IGNORE) 静默跳过（此前会带着
        // v1 版本号"成功"返回）。
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER wp1c2a_ignore_meta BEFORE UPDATE ON meta
             BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    }
    let err = migrate_v1_to_v2(&db, &backup).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");

    // 源库仍为 v1：launch_fences 建表随事务回滚，历史数据完好。
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "1");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 0);
    assert_eq!(table_count(&probe, "tasks"), 1);
    assert_eq!(table_count(&probe, "events"), 2);
    drop(probe);

    // 备份仍产出且可读（v1）。
    let backup_conn = Connection::open(&backup).unwrap();
    assert_eq!(meta_version(&backup_conn), "1");
    assert_eq!(table_count(&backup_conn, "tasks"), 1);
    drop(backup_conn);

    // 撤销注入后重试成功（换新备份路径，不覆盖已有备份）。
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER wp1c2a_ignore_meta;")
        .unwrap();
    drop(conn);
    let retry_backup = dir.join("backup-v1-retry.db");
    migrate_v1_to_v2(&db, &retry_backup).unwrap();
    migrate_v2_to_v3(&db, &dir.join("backup-v3-retry.db")).unwrap();
    migrate_v3_to_v4(&db, &dir.join("backup-v4-retry.db")).unwrap();
    migrate_v4_to_v5(&db, &dir.join("backup-v5-retry.db")).unwrap();
    migrate_v5_to_v6(&db, &dir.join("backup-v6-retry.db")).unwrap();
    migrate_v6_to_v7(&db, &dir.join("backup-v7-retry.db")).unwrap();
    migrate_v7_to_v8(&db, &dir.join("backup-v8-retry.db")).unwrap();
    migrate_v8_to_v9(&db, &dir.join("backup-v9-retry.db")).unwrap();
    let probe = Connection::open(&db).unwrap();
    assert_eq!(meta_version(&probe), "9");
    assert_eq!(sqlite_master_count(&probe, "launch_fences"), 1);

    remove_dir(&dir);
}

#[test]
fn migrate_nonexistent_source_errors_without_creating_files() {
    let dir = temp_dir("c2a-missing-source");
    let db = dir.join("relay.db");
    let backup = dir.join("backup-v1.db");
    let err = migrate_v1_to_v2(&db, &backup).unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");
    assert!(
        err.to_string().contains("unable to open database file"),
        "应为源库打不开的底层错误: {err}"
    );
    // 目录仍为空：未创建源库、备份或 SQLite 侧车文件。
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.is_empty(), "目录不应有任何文件: {names:?}");

    remove_dir(&dir);
}

/// 把任务及其 attempt 1 搬到 failed（revision 不变），供重试测试staging。
fn force_failed(store: &Store, task_id: &str) {
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'failed' WHERE task_id = '{task_id}';
             UPDATE attempts SET state = 'failed' WHERE task_id = '{task_id}' AND attempt_no = 1;"
        ))
        .unwrap();
}

#[test]
fn retry_creates_new_attempt_and_preserves_history() {
    let dir = temp_dir("d1-retry");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    // 场景一：failed 任务重试。
    let (t1, req1, plan1) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &t1,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    force_failed(&store, &t1);
    let plan2 = format!("{plan1}-r");
    store.put_plan(plan_input(&plan2, T0, TFAR)).unwrap();
    let outcome = store
        .retry(RetryInput {
            task_id: &t1,
            plan_id: &plan2,
            request_id: "req_retry1",
            expected_revision: 2,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    assert!(!outcome.deduplicated);
    assert_eq!(outcome.state, State::Queued);
    assert_eq!(outcome.revision, 3);
    assert_eq!(outcome.active_attempt, 2);
    assert_eq!(outcome.created_at, T15);

    // 逐表核对：task 指针推进，新旧 attempt 并存，事件与快照齐备。
    let task = store.get_task(&t1).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, 3);
    assert_eq!(task.active_attempt, 2);
    assert_eq!(task.updated_at, T15);
    let rows: Vec<(i64, String, i64, String)> = store
        .conn()
        .unwrap()
        .prepare("SELECT attempt_no, state, revision, created_at FROM attempts WHERE task_id = ?1 ORDER BY attempt_no")
        .unwrap()
        .query_map(rusqlite::params![t1], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0, rows[0].1.as_str(), rows[0].2), (1, "failed", 2));
    assert_eq!(
        (rows[1].0, rows[1].1.as_str(), rows[1].2, rows[1].3.as_str()),
        (2, "queued", 3, T15)
    );
    assert_eq!(count_events(&store, &t1), 3);
    let (kind, attempt_no, rev, ev_state): (String, i64, i64, String) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT kind, attempt_no, revision, state FROM events WHERE task_id = ?1 AND revision = 3",
            rusqlite::params![t1],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), attempt_no, rev, ev_state.as_str()),
        ("submitted", 2, 3, "queued")
    );
    let retry_req = store.get_request_result("req_retry1").unwrap().unwrap();
    assert_eq!(retry_req.operation, OP_RETRY);
    assert_eq!(retry_req.result_json, outcome.result_json);
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT status FROM plans WHERE plan_id = '{plan1}'")
        ),
        "used"
    );
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT status FROM plans WHERE plan_id = '{plan2}'")
        ),
        "used"
    );
    // 首提快照不变。
    let original = store.submit(submit_input(&req1, &plan1, T60)).unwrap();
    assert!(original.deduplicated);
    assert_eq!(original.state, State::Queued);
    assert_eq!(original.revision, 1);
    assert_eq!(original.active_attempt, 1);

    // 场景二：栅栏记录在重试后保留（launching → failed → retry）。
    let (t2, _, plan_t2) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &t2,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    store
        .launch_fence(LaunchFenceInput {
            task_id: &t2,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "code-d1",
            worktree_id: "wt-d1",
            now: T15,
        })
        .unwrap();
    force_failed(&store, &t2);
    let plan3 = format!("{plan_t2}-r");
    store.put_plan(plan_input(&plan3, T0, TFAR)).unwrap();
    let outcome2 = store
        .retry(RetryInput {
            task_id: &t2,
            plan_id: &plan3,
            request_id: "req_retry2",
            expected_revision: 3,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome2.active_attempt, 2);
    // 旧 attempt 的栅栏记录不变。
    let fence = store.get_launch_fence(&t2, 1).unwrap().unwrap();
    assert_eq!(fence.launch_code, "code-d1");
    assert!(store.get_launch_fence(&t2, 2).unwrap().is_none());

    remove_dir(&dir);
}

#[test]
fn retry_replays_and_blocks_plan_reuse() {
    let dir = temp_dir("d1-replay");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    let (t, _req_s, plan_s) = enqueue_one(&store, T0);
    force_failed(&store, &t);
    let plan_r = format!("{plan_s}-r");
    store.put_plan(plan_input(&plan_r, T0, T15)).unwrap();
    let first = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: &plan_r,
            request_id: "req_retry_r",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap();
    assert_eq!(first.revision, 2);
    assert_eq!(first.active_attempt, 2);

    // 重开库后重放：plan 已消费且（按 T60）已过期，仍返回首次重试结果。
    let reopened = Store::open(&db).unwrap();
    let replay = reopened
        .retry(RetryInput {
            task_id: &t,
            plan_id: &plan_r,
            request_id: "req_retry_r",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T60,
        })
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.result_json, first.result_json);
    assert_eq!(replay.state, State::Queued);
    assert_eq!(replay.revision, 2);
    assert_eq!(replay.active_attempt, 2);
    // 实时状态由 get_task 提供（本场景无状态推进，仍为 queued/2）。
    let task = reopened.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Queued);
    assert_eq!(task.revision, 2);

    // 同 ID 异参（换 plan）→ IDEMPOTENCY_CONFLICT。
    let plan_r2 = format!("{plan_s}-r2");
    reopened.put_plan(plan_input(&plan_r2, T0, TFAR)).unwrap();
    let err = reopened
        .retry(RetryInput {
            task_id: &t,
            plan_id: &plan_r2,
            request_id: "req_retry_r",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(err, StoreError::IdempotencyConflict { .. }),
        "{err:?}"
    );

    // 已消费 plan 不得再被首次提交或第二次重试使用。
    // 先把任务搬回 failed（revision 仍为 2），使资格检查放行、走到 plan 校验。
    force_failed(&reopened, &t);
    for plan in [&plan_s, &plan_r] {
        let err = reopened
            .retry(RetryInput {
                task_id: &t,
                plan_id: plan,
                request_id: "req_retry_reuse",
                expected_revision: 2,
                no_active_worker_confirmed: false,
                now: T0,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::PlanUsed { .. }), "{err:?}");
    }
    // 首次提交路径同样不能重用已消费 plan。
    let err = reopened
        .submit(submit_input("req_submit_reuse", &plan_s, T0))
        .unwrap_err();
    assert!(matches!(err, StoreError::PlanUsed { .. }), "{err:?}");

    remove_dir(&dir);
}

#[test]
fn retry_requires_same_scope_rules_and_profile() {
    let dir = temp_dir("d1-retry-conditions");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let (task, _, plan) = enqueue_one(&store, T0);
    force_failed(&store, &task);

    // Original plan from enqueue_one has src/count.rs, cargo_test and
    // prof_mock. Any widened scope, added rule, or different profile must be
    // rejected before a new attempt is created.
    let variants: [(&str, &str, &str, &str); 3] = [
        (
            "scope",
            "[\"src/count.rs\",\"tests/other.rs\"]",
            "[\"cargo_test\"]",
            "prof_mock",
        ),
        (
            "rules",
            "[\"src/count.rs\"]",
            "[\"cargo_test\",\"extra_test\"]",
            "prof_mock",
        ),
        (
            "profile",
            "[\"src/count.rs\"]",
            "[\"cargo_test\"]",
            "prof_other",
        ),
    ];
    for (name, scope, rules, profile) in variants {
        let plan_id = format!("{plan}-{name}");
        store
            .put_plan(PlanInput {
                plan_id: &plan_id,
                project_id: "proj_f6a2",
                goal: "修复块计数在空输入时的返回值",
                brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
                base_oid: BASE_OID,
                created_at: T0,
                expires_at: TFAR,
                write_scope_json: scope,
                rule_ids_json: rules,
                profile_id: profile,
            })
            .unwrap();
        let err = store
            .retry(RetryInput {
                task_id: &task,
                plan_id: &plan_id,
                request_id: &format!("req_retry_condition_{name}"),
                expected_revision: 1,
                no_active_worker_confirmed: false,
                now: T15,
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::PlanStale { .. }),
            "{name}: {err:?}"
        );
        assert_eq!(store.get_task(&task).unwrap().unwrap().active_attempt, 1);
        assert_eq!(
            scalar(
                &store,
                &format!("SELECT status FROM plans WHERE plan_id = '{plan_id}'")
            ),
            "unused"
        );
    }

    remove_dir(&dir);
}

#[test]
fn retry_rejections_cover_eligibility_and_conditions() {
    let dir = temp_dir("d1-reject");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let (t_failed, _, plan_ok) = enqueue_one(&store, T0);
    force_failed(&store, &t_failed);
    // plan_ok 已由 enqueue_one 登记为远期过期，可直接复用。

    // CAS 前提：revision 预期不符（实际 1）。
    for expected in [2u64, 3] {
        let err = store
            .retry(RetryInput {
                task_id: &t_failed,
                plan_id: &plan_ok,
                request_id: "req_stale",
                expected_revision: expected,
                no_active_worker_confirmed: false,
                now: T0,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::StaleRevision { .. }), "{err:?}");
    }

    // 资格：queued、preparing、completed 不可重试。
    let (t_queued, _, _) = enqueue_one(&store, T0);
    let err = store
        .retry(RetryInput {
            task_id: &t_queued,
            plan_id: &plan_ok,
            request_id: "req_queued",
            expected_revision: 1,
            no_active_worker_confirmed: true,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::RetryNotAllowed {
                state: State::Queued,
                ..
            }
        ),
        "{err:?}"
    );
    let (t_preparing, _, _) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &t_preparing,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t_preparing,
            plan_id: &plan_ok,
            request_id: "req_preparing",
            expected_revision: 2,
            no_active_worker_confirmed: true,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::RetryNotAllowed {
                state: State::Preparing,
                ..
            }
        ),
        "{err:?}"
    );
    let (t_completed, _, _) = enqueue_one(&store, T0);
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'completed' WHERE task_id = '{t_completed}';
             UPDATE attempts SET state = 'completed' WHERE task_id = '{t_completed}' AND attempt_no = 1;"
        ))
        .unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t_completed,
            plan_id: &plan_ok,
            request_id: "req_completed",
            expected_revision: 1,
            no_active_worker_confirmed: true,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::RetryNotAllowed {
                state: State::Completed,
                ..
            }
        ),
        "{err:?}"
    );

    // 中断：未确认无活动 worker 时拒绝；确认后允许。
    let (t_interrupted, _, _) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &t_interrupted,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE tasks SET state = 'interrupted' WHERE task_id = '{t_interrupted}';
             UPDATE attempts SET state = 'interrupted' WHERE task_id = '{t_interrupted}' AND attempt_no = 1;"
        ))
        .unwrap();
    let plan_i = format!("{plan_ok}-i");
    store.put_plan(plan_input(&plan_i, T0, TFAR)).unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t_interrupted,
            plan_id: &plan_i,
            request_id: "req_interrupted",
            expected_revision: 2,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::RetryNotAllowed {
                state: State::Interrupted,
                ..
            }
        ),
        "{err:?}"
    );
    let outcome = store
        .retry(RetryInput {
            task_id: &t_interrupted,
            plan_id: &plan_i,
            request_id: "req_interrupted_ok",
            expected_revision: 2,
            no_active_worker_confirmed: true,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome.active_attempt, 2);

    // 新 plan 条件不符：project / base_oid / goal 任一不一致 → PLAN_STALE。
    let plan_bad_project = PlanInput {
        plan_id: "plan_bad_project",
        project_id: "proj_other",
        goal: "修复块计数在空输入时的返回值",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: BASE_OID,
        created_at: T0,
        expires_at: TFAR,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_bad_project).unwrap();
    let plan_bad_base = PlanInput {
        plan_id: "plan_bad_base",
        project_id: "proj_f6a2",
        goal: "修复块计数在空输入时的返回值",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: "ffffffffffffffffffffffffffffffffffffffff",
        created_at: T0,
        expires_at: TFAR,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_bad_base).unwrap();
    let plan_bad_goal = PlanInput {
        plan_id: "plan_bad_goal",
        project_id: "proj_f6a2",
        goal: "另一个目标",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: BASE_OID,
        created_at: T0,
        expires_at: TFAR,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_bad_goal).unwrap();
    for plan in ["plan_bad_project", "plan_bad_base", "plan_bad_goal"] {
        let err = store
            .retry(RetryInput {
                task_id: &t_failed,
                plan_id: plan,
                request_id: "req_stale_plan",
                expected_revision: 1,
                no_active_worker_confirmed: false,
                now: T0,
            })
            .unwrap_err();
        assert!(
            matches!(err, StoreError::PlanStale { .. }),
            "plan {plan} 应报 PLAN_STALE: {err:?}"
        );
        assert_eq!(err.code(), "PLAN_STALE");
    }

    // 计划过期、未知计划、缺失任务。
    let plan_expired = PlanInput {
        plan_id: "plan_expired",
        project_id: "proj_f6a2",
        goal: "修复块计数在空输入时的返回值",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: BASE_OID,
        created_at: T0,
        expires_at: T15,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_expired).unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t_failed,
            plan_id: "plan_expired",
            request_id: "req_expired",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T60,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::PlanExpired { .. }), "{err:?}");
    let err = store
        .retry(RetryInput {
            task_id: &t_failed,
            plan_id: "plan_missing",
            request_id: "req_unknown_plan",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidInput(_)), "{err:?}");
    let err = store
        .retry(RetryInput {
            task_id: "task_nope",
            plan_id: &plan_ok,
            request_id: "req_missing_task",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");

    // 全部拒绝后：failed 任务零写入（attempt 仍 1 行、rev 1、无新事件/请求）。
    let task = store.get_task(&t_failed).unwrap().unwrap();
    assert_eq!(task.state, State::Failed);
    assert_eq!(task.revision, 1);
    assert_eq!(attempt_row(&store, &t_failed), ("failed".into(), 1));
    assert_eq!(count_events(&store, &t_failed), 1);
    assert!(store
        .get_request_result("req_stale_plan")
        .unwrap()
        .is_none());

    remove_dir(&dir);
}

#[test]
fn retry_concurrent_and_injection_rolls_back() {
    let dir = temp_dir("d1-race");
    let db = dir.join("relay.db");
    // 场景 A：双连接同旧 revision 竞争重试 → 恰好一次成功，plan 只消费一次。
    let (t, _, plan) = enqueue_one(&Store::open(&db).unwrap(), T0);
    force_failed(&Store::open(&db).unwrap(), &t);
    let plan_retry = format!("{plan}-r");
    {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input(&plan_retry, T0, TFAR)).unwrap();
    }
    let results: Vec<Result<SubmitOutcome, StoreError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = ["req_race_a", "req_race_b"]
            .iter()
            .map(|request_id| {
                let db = &db;
                let t = t.clone();
                let plan_retry = plan_retry.clone();
                scope.spawn(move || {
                    let store = Store::open(db).unwrap();
                    store.retry(RetryInput {
                        task_id: &t,
                        plan_id: &plan_retry,
                        request_id,
                        expected_revision: 1,
                        no_active_worker_confirmed: false,
                        now: T15,
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut ok_count = 0;
    let mut stale_count = 0;
    for result in results {
        match result {
            Ok(outcome) => {
                ok_count += 1;
                assert_eq!(outcome.active_attempt, 2);
                assert_eq!(outcome.revision, 2);
            }
            Err(StoreError::StaleRevision { .. }) => stale_count += 1,
            Err(err) => panic!("意外的错误: {err:?}"),
        }
    }
    assert_eq!((ok_count, stale_count), (1, 1));
    {
        let store = Store::open(&db).unwrap();
        let task = store.get_task(&t).unwrap().unwrap();
        assert_eq!(
            (task.state, task.revision, task.active_attempt),
            (State::Queued, 2, 2)
        );
        assert_eq!(count(&store, "attempts"), 2);
        assert_eq!(count_events(&store, &t), 2);
        assert_eq!(
            scalar(
                &store,
                &format!("SELECT status FROM plans WHERE plan_id = '{plan_retry}'")
            ),
            "used",
            "plan 只消费一次"
        );
        assert_eq!(count(&store, "requests"), 2);
    }

    // 场景 B：事件 ABORT 注入 → 整笔回滚，无新 attempt、plan 未消费。
    let (t2, _, plan2) = enqueue_one(&Store::open(&db).unwrap(), T0);
    force_failed(&Store::open(&db).unwrap(), &t2);
    let plan2_retry = format!("{plan2}-r");
    {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input(&plan2_retry, T0, TFAR)).unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER wp1d1_abort BEFORE INSERT ON events
                 WHEN new.task_id = '{t2}' AND new.attempt_no = 2
                 BEGIN SELECT RAISE(ABORT, 'injected retry failure'); END;"
            ))
            .unwrap();
    }
    let store = Store::open(&db).unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t2,
            plan_id: &plan2_retry,
            request_id: "req_retry_abort",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Db(_)), "{err:?}");
    let task = store.get_task(&t2).unwrap().unwrap();
    assert_eq!(task.state, State::Failed);
    assert_eq!(task.revision, 1);
    assert_eq!(attempt_row(&store, &t2), ("failed".into(), 1));
    assert_eq!(count(&store, "attempts"), 3); // t(2) + t2(1)
    assert_eq!(count_events(&store, &t2), 1);
    assert!(store
        .get_request_result("req_retry_abort")
        .unwrap()
        .is_none());
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT status FROM plans WHERE plan_id = '{plan2_retry}'")
        ),
        "unused"
    );
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1d1_abort;")
        .unwrap();
    let outcome = store
        .retry(RetryInput {
            task_id: &t2,
            plan_id: &plan2_retry,
            request_id: "req_retry_abort",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome.active_attempt, 2);
    assert_eq!(count_events(&store, &t2), 2);

    // 场景 C：IGNORE 注入 → 行数校验拒绝，整笔回滚。
    let (t3, _, plan3) = enqueue_one(&Store::open(&db).unwrap(), T0);
    force_failed(&Store::open(&db).unwrap(), &t3);
    let plan3_retry = format!("{plan3}-r");
    {
        let store = Store::open(&db).unwrap();
        store.put_plan(plan_input(&plan3_retry, T0, TFAR)).unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER wp1d1_ignore BEFORE INSERT ON events
                 WHEN new.task_id = '{t3}' AND new.attempt_no = 2
                 BEGIN SELECT RAISE(IGNORE); END;"
            ))
            .unwrap();
    }
    let err = store
        .retry(RetryInput {
            task_id: &t3,
            plan_id: &plan3_retry,
            request_id: "req_retry_ignore",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    let task = store.get_task(&t3).unwrap().unwrap();
    assert_eq!(task.state, State::Failed);
    assert_eq!(task.revision, 1);
    assert_eq!(count(&store, "attempts"), 5); // t(2) + t2(2) + t3(1)
    assert_eq!(count_events(&store, &t3), 1);
    assert!(store
        .get_request_result("req_retry_ignore")
        .unwrap()
        .is_none());
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT status FROM plans WHERE plan_id = '{plan3_retry}'")
        ),
        "unused"
    );
    store
        .conn()
        .unwrap()
        .execute_batch("DROP TRIGGER wp1d1_ignore;")
        .unwrap();
    let outcome = store
        .retry(RetryInput {
            task_id: &t3,
            plan_id: &plan3_retry,
            request_id: "req_retry_ignore",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    assert_eq!(outcome.active_attempt, 2);

    remove_dir(&dir);
}

#[test]
fn claim_next_orders_retries_after_already_queued() {
    let dir = temp_dir("d2-order");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    // 三个任务按 seq 入队（提交时间故意乱序）。
    let a = enqueue_one(&store, T60);
    let b = enqueue_one(&store, T0);
    let c = enqueue_one(&store, T15);
    // 领取 A、B，把 B 搬到 failed 后重试：B 的新 attempt 带更大的入队 seq。
    for t in [&a.0, &b.0] {
        store
            .transition(TransitionInput {
                task_id: t,
                expected_active_attempt: 1,
                expected_revision: 1,
                event: Event::Claim,
                now: T15,
            })
            .unwrap();
    }
    force_failed(&store, &b.0);
    let plan_b_retry = format!("{}-r", b.2);
    store.put_plan(plan_input(&plan_b_retry, T0, TFAR)).unwrap();
    let retried = store
        .retry(RetryInput {
            task_id: &b.0,
            plan_id: &plan_b_retry,
            request_id: "req_d2_retry_b",
            expected_revision: 2,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    assert_eq!(retried.active_attempt, 2);
    drop(store);

    // 重开库：顺序领取——已入队的 C 必须先于重试的 B（确定性顺序证明）。
    let store = Store::open(&db).unwrap();
    let first = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(
        (first.task_id.as_str(), first.active_attempt),
        (c.0.as_str(), 1)
    );
    drop(store);

    // 双连接并发领取剩余队列：恰好一个拿到重试的 B，另一个拿到 None。
    let results: Vec<Result<Option<TransitionOutcome>, StoreError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let db = &db;
                scope.spawn(move || {
                    let store = Store::open(db).unwrap();
                    store.claim_next(T15)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    let mut some_count = 0;
    let mut none_count = 0;
    for result in results {
        match result.unwrap() {
            Some(outcome) => {
                some_count += 1;
                assert_eq!(
                    (
                        outcome.task_id.as_str(),
                        outcome.active_attempt,
                        outcome.revision
                    ),
                    (b.0.as_str(), 2, 4)
                );
            }
            None => none_count += 1,
        }
    }
    assert_eq!((some_count, none_count), (1, 1));
    let store = Store::open(&db).unwrap();
    let task = store.get_task(&b.0).unwrap().unwrap();
    assert_eq!(task.state, State::Preparing);
    assert_eq!(task.active_attempt, 2);
    assert_eq!(task.revision, 4);
    // 每个任务只追加一条 claim 事件（attempt_no 等于当前 attempt）。
    for (task_id, attempt_no) in [(&c.0, 1), (&b.0, 2)] {
        let claims: i64 = store
            .conn()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND kind = 'claim' AND attempt_no = ?2",
                rusqlite::params![task_id, i64::from(attempt_no)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1);
    }
    // 队列耗尽。
    assert!(store.claim_next(T15).unwrap().is_none());
    drop(store);

    // 重试 attempt 的队首损坏：缺当前 attempt 入队事件时拒绝，不跳过后条。
    let db2 = dir.join("relay2.db");
    let store = Store::open(&db2).unwrap();
    let (g, _, _) = enqueue_one(&store, T0);
    store
        .transition(TransitionInput {
            task_id: &g,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T15,
        })
        .unwrap();
    force_failed(&store, &g);
    let plan_g = format!("{}-r", g.clone());
    store.put_plan(plan_input(&plan_g, T0, TFAR)).unwrap();
    store
        .retry(RetryInput {
            task_id: &g,
            plan_id: &plan_g,
            request_id: "req_d2_retry_g",
            expected_revision: 2,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap();
    let (h, _, _) = enqueue_one(&store, T0);
    // 记录 G 当前 attempt 入队事件的 seq，删除后按原 seq 还原。
    let g_seq: i64 = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT seq FROM events WHERE task_id = ?1 AND kind = 'submitted' AND attempt_no = 2",
            rusqlite::params![g],
            |r| r.get(0),
        )
        .unwrap();
    store
        .conn()
        .unwrap()
        .execute_batch(&format!("DELETE FROM events WHERE seq = {g_seq};"))
        .unwrap();
    let err = store.claim_next(T15).unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    // G 与 H 都未变。
    for t in [&g, &h] {
        let task = store.get_task(t).unwrap().unwrap();
        assert_eq!(task.state, State::Queued);
    }
    assert_eq!(count(&store, "events"), 3); // G(2，入队事件已删) + H(1)
                                            // 还原后先领 G（重试 attempt），再领 H。
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "INSERT INTO events (seq, task_id, attempt_no, revision, kind, state, reason_code, created_at)
             VALUES ({g_seq}, '{g}', 2, 3, 'submitted', 'queued', NULL, '{T15}');"
        ))
        .unwrap();
    let first = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(
        (first.task_id.as_str(), first.active_attempt),
        (g.as_str(), 2)
    );
    let second = store.claim_next(T15).unwrap().unwrap();
    assert_eq!(
        (second.task_id.as_str(), second.active_attempt),
        (h.as_str(), 1)
    );

    remove_dir(&dir);
}

#[test]
fn retry_rejects_brief_only_change_and_reports_plan_used_task() {
    let dir = temp_dir("d1-fix-brief");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let (t, _req_s, plan_s) = enqueue_one(&store, T0);
    force_failed(&store, &t);

    // 只改 brief 的 plan：其余条件一致，仍必须报 PLAN_STALE。
    let plan_brief_only = PlanInput {
        plan_id: "plan_brief_only",
        project_id: "proj_f6a2",
        goal: "修复块计数在空输入时的返回值",
        brief: "改过的简报文本",
        base_oid: BASE_OID,
        created_at: T0,
        expires_at: TFAR,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_brief_only).unwrap();
    let err = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: "plan_brief_only",
            request_id: "req_brief_only",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::PlanStale { .. }), "{err:?}");
    assert_eq!(err.code(), "PLAN_STALE");
    // 零写入：plan 未消费、无新 attempt、无请求。
    assert_eq!(
        scalar(
            &store,
            "SELECT status FROM plans WHERE plan_id = 'plan_brief_only'"
        ),
        "unused"
    );
    assert_eq!(count(&store, "attempts"), 1);
    assert!(store
        .get_request_result("req_brief_only")
        .unwrap()
        .is_none());
    assert_eq!(count_events(&store, &t), 1);

    // 场景 A：首提消费的 plan 用新 request_id 重试 → PLAN_USED 带回 task_id。
    let err = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: &plan_s,
            request_id: "req_reuse_submit_plan",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::PlanUsed {
                existing_task_id: Some(ref task_id),
                ..
            } if task_id == &t
        ),
        "{err:?}"
    );

    // 场景 B：重试消费的 plan（条件一致的 plan_retry）再用新 request_id
    // 重试 → PLAN_USED 带回 task_id。
    let plan_ok = PlanInput {
        plan_id: "plan_retry_ok",
        project_id: "proj_f6a2",
        goal: "修复块计数在空输入时的返回值",
        brief: "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
        base_oid: BASE_OID,
        created_at: T0,
        expires_at: TFAR,
        write_scope_json: "[\"src/count.rs\"]",
        rule_ids_json: "[\"cargo_test\"]",
        profile_id: "prof_mock",
    };
    store.put_plan(plan_ok).unwrap();
    let outcome = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: "plan_retry_ok",
            request_id: "req_retry_ok",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap();
    assert_eq!(outcome.active_attempt, 2);
    // 重试成功后任务回到 queued，搬回 failed 使资格放行、走到 plan 校验。
    force_failed(&store, &t);
    let err = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: "plan_retry_ok",
            request_id: "req_reuse_retry_plan",
            expected_revision: 2,
            no_active_worker_confirmed: false,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::PlanUsed {
                existing_task_id: Some(ref task_id),
                ..
            } if task_id == &t
        ),
        "{err:?}"
    );

    remove_dir(&dir);
}

#[test]
fn retry_attempt_number_overflow_is_explicit_error() {
    let dir = temp_dir("d1-fix-overflow");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    let (t, req_s, plan_s) = enqueue_one(&store, T0);
    // stage：attempt 编号推到 u32::MAX，状态 failed（可重试），revision 1。
    force_failed(&store, &t);
    store
        .conn()
        .unwrap()
        .execute_batch(&format!(
            "UPDATE attempts SET attempt_no = {max} WHERE task_id = '{t}' AND attempt_no = 1;
             UPDATE tasks SET active_attempt = {max} WHERE task_id = '{t}';",
            max = i64::from(u32::MAX)
        ))
        .unwrap();
    let plan_r = format!("{plan_s}-r");
    store.put_plan(plan_input(&plan_r, T0, TFAR)).unwrap();
    let before = fs::read(&db).unwrap();

    let err = store
        .retry(RetryInput {
            task_id: &t,
            plan_id: &plan_r,
            request_id: "req_overflow",
            expected_revision: 1,
            no_active_worker_confirmed: false,
            now: T15,
        })
        .unwrap_err();
    assert!(matches!(err, StoreError::Environment(_)), "{err:?}");
    assert!(err.to_string().contains("u32 上限"), "{err}");

    // 零写入：无新 attempt、plan 未消费、无请求、事件不变、文件字节未变。
    assert_eq!(fs::read(&db).unwrap(), before, "拒绝不得修改数据库文件");
    assert_eq!(count(&store, "attempts"), 1);
    let attempt_no: i64 = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT attempt_no FROM attempts WHERE task_id = ?1",
            rusqlite::params![t],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempt_no, i64::from(u32::MAX), "不允许截断为 0");
    assert_eq!(
        scalar(
            &store,
            &format!("SELECT status FROM plans WHERE plan_id = '{plan_r}'")
        ),
        "unused"
    );
    assert!(store.get_request_result("req_overflow").unwrap().is_none());
    assert_eq!(count_events(&store, &t), 1);
    let task = store.get_task(&t).unwrap().unwrap();
    assert_eq!(task.state, State::Failed);
    assert_eq!(task.active_attempt, u32::MAX);
    // 同一 Store 仍可正常读取。
    assert!(store.get_request_result(&req_s).unwrap().is_some());

    remove_dir(&dir);
}

#[test]
fn stage_permit_is_single_use_and_recovers_without_replay() {
    use relay_store::{StageKey, StageKind};
    use std::sync::Barrier;
    let dir = temp_dir("stage-permit");
    let db = dir.join("relay.db");
    let store = Store::open(&db).unwrap();
    let task_id = claim_one(&store);
    let (next_task, _, _) = enqueue_one(&store, T15);
    store
        .launch_fence(LaunchFenceInput {
            task_id: &task_id,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: "stage-launch",
            worktree_id: "stage-wt",
            now: T15,
        })
        .unwrap();
    let implement = StageKey {
        task_id: &task_id,
        attempt_no: 1,
        round: 1,
        kind: StageKind::Implement,
    };
    let verify = StageKey {
        kind: StageKind::Verify,
        ..implement
    };
    assert!(store.begin_stage(verify, 3, None, T15).is_err());
    let barrier = Barrier::new(2);
    let winners = std::thread::scope(|scope| {
        let race = || {
            let other = Store::open(&db).unwrap();
            barrier.wait();
            other.begin_stage(implement, 3, None, T15).is_ok()
        };
        let a = scope.spawn(race);
        let b = scope.spawn(race);
        usize::from(a.join().unwrap()) + usize::from(b.join().unwrap())
    });
    assert_eq!(winners, 1);
    drop(store);
    let store = Store::open(&db).unwrap();
    assert!(store.has_active_stage(&task_id, 1).unwrap());
    assert_eq!(store.recover(T60).unwrap()[0].state, State::Interrupted);
    let stage: (String, String) = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT state, ended_at FROM stage_runs WHERE task_id=?1",
            [&task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stage, ("interrupted".into(), T60.into()));
    assert!(store.begin_stage(implement, 4, None, T60).is_err());
    assert!(store.begin_stage(verify, 4, None, T60).is_err());
    assert!(store.recover(T60).unwrap().is_empty());
    assert_eq!(store.list_pending_gates().unwrap().len(), 1);
    assert!(store.claim_next(T60).unwrap().is_none());
    assert_eq!(
        store.get_task(&next_task).unwrap().unwrap().state,
        State::Queued
    );
    drop(store);
    remove_dir(&dir);
}

/// A19：已有实例缺表/缺列时必须拒绝打开（可写与只读一致），不得静默补建。
#[test]
fn missing_required_table_or_column_is_rejected() {
    let dir = temp_dir("schema-full-check");
    let path = dir.join("relay.db");
    {
        let store = Store::open(&path).unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch("DROP TABLE stage_runs")
            .unwrap();
    }
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::Environment(_))
    ));
    assert!(matches!(
        Store::open_read_only(&path),
        Err(StoreError::Environment(_))
    ));

    // Missing column in an existing table is rejected too.
    let path2 = dir.join("relay2.db");
    {
        let store = Store::open(&path2).unwrap();
        store
            .conn()
            .unwrap()
            .execute_batch("ALTER TABLE tasks DROP COLUMN reason_code")
            .unwrap();
    }
    assert!(matches!(
        Store::open(&path2),
        Err(StoreError::Environment(_))
    ));
    fs::remove_dir_all(&dir).ok();
}
