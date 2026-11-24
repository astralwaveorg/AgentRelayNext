//! M1 schema v3 契约测试：制品登记与状态同事务、未登记不可见、
//! events.list、relay_list 分页与显式验收摘要门。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use relay_core::{Event, State};
use relay_store::{ArtifactRegisterInput, ReviewInput, Store, SubmitInput, TransitionInput};

const T0: &str = "2026-11-22T01:00:00Z";
const T60: &str = "2026-11-22T02:00:00Z";
const OID: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-store-v3-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 提交任务并推进到 running，返回生成的 task_id。
fn stage_running_task(store: &Store, plan_id: &str, request_id: &str) -> String {
    store
        .put_plan(relay_store::PlanInput {
            plan_id,
            project_id: "proj_v3",
            goal: "目标",
            brief: "简报",
            base_oid: OID,
            created_at: T0,
            expires_at: T60,
            write_scope_json: "[]",
            rule_ids_json: "[]",
            profile_id: "",
        })
        .unwrap();
    let outcome = store
        .submit(SubmitInput {
            request_id,
            plan_id,
            now: T0,
        })
        .unwrap();
    let task_id = outcome.task_id.clone();
    // queued → preparing（Claim）
    store
        .transition(TransitionInput {
            task_id: &task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: Event::Claim,
            now: T0,
        })
        .unwrap();
    // preparing → launching 必须经启动栅栏专用方法（03）。
    store
        .launch_fence(relay_store::LaunchFenceInput {
            task_id: &task_id,
            expected_active_attempt: 1,
            expected_revision: 2,
            launch_code: &format!("launch-{plan_id}-1"),
            worktree_id: &format!("wt-{plan_id}"),
            now: T0,
        })
        .unwrap();
    // launching → running
    store
        .transition(TransitionInput {
            task_id: &task_id,
            expected_active_attempt: 1,
            expected_revision: 3,
            event: Event::Spawned,
            now: T0,
        })
        .unwrap();
    task_id
}

fn verify_transition(store: &Store, task_id: &str, revision: u64) {
    store
        .transition(TransitionInput {
            task_id,
            expected_active_attempt: 1,
            expected_revision: revision,
            event: Event::WorkerSucceeded,
            now: T0,
        })
        .unwrap();
}

#[test]
fn register_artifact_and_transition_are_atomic() {
    let dir = temp_dir("register");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let task_id = stage_running_task(&store, "plan_r1", "req_r1");
    verify_transition(&store, &task_id, 4);

    // 登记制品 + VerificationPassed 同事务：verifying → awaiting_review。
    let outcome = store
        .register_artifact_and_transition(ArtifactRegisterInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 5,
            artifact_dir: "/managed/artifacts/candidate",
            manifest_sha256: SHA,
            patch_sha256: SHA,
            receipt_sha256: SHA,
            handoff_sha256: SHA,
            now: T0,
        })
        .unwrap();
    assert_eq!(outcome.state, State::AwaitingReview);
    assert_eq!(outcome.revision, 6);

    // 登记后查询可见（唯一事实来源）。
    let record = store.get_artifact(&task_id, 1).unwrap().unwrap();
    assert_eq!(record.artifact_dir, "/managed/artifacts/candidate");
    assert_eq!(record.patch_sha256, SHA);

    // 未登记 attempt 查询返回 None（未登记目录不可被发现）。
    assert!(store.get_artifact(&task_id, 2).unwrap().is_none());
    assert!(store.get_artifact("task_other", 1).unwrap().is_none());

    // 已 awaiting_review 的任务重复登记：状态机先行拒绝（WrongState），
    // 制品主键兜底防同 attempt 双行。
    let err = store
        .register_artifact_and_transition(ArtifactRegisterInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 6,
            artifact_dir: "/managed/artifacts/again",
            manifest_sha256: SHA,
            patch_sha256: SHA,
            receipt_sha256: SHA,
            handoff_sha256: SHA,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(err, relay_store::StoreError::WrongState { .. }),
        "重复登记应拒绝: {err:?}"
    );

    remove_dir(&dir);
}

#[test]
fn register_failure_rolls_back_state() {
    let dir = temp_dir("rollback");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let task_id = stage_running_task(&store, "plan_r2", "req_r2");
    verify_transition(&store, &task_id, 4);

    // 非法摘要：登记失败，状态推进同事务回滚——任务保持 verifying，
    // 无半提交状态，事件未追加。
    let err = store
        .register_artifact_and_transition(ArtifactRegisterInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 5,
            artifact_dir: "/managed/artifacts/candidate",
            manifest_sha256: "NOT-A-SHA",
            patch_sha256: SHA,
            receipt_sha256: SHA,
            handoff_sha256: SHA,
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(err, relay_store::StoreError::InvalidInput(_)),
        "{err:?}"
    );
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.state, State::Verifying);
    assert_eq!(task.revision, 5);
    assert!(store.get_artifact(&task_id, 1).unwrap().is_none());

    remove_dir(&dir);
}

#[test]
fn events_list_pages_forward() {
    let dir = temp_dir("events");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    stage_running_task(&store, "plan_e1", "req_e1");

    // 全量事件从 0 开始分页。
    let first = store.list_events(0, 3).unwrap();
    assert_eq!(first.len(), 3);
    assert_eq!(first[0].seq, 1);
    assert_eq!(first[0].kind, "submitted");
    let after = first.last().unwrap().seq;
    let second = store.list_events(after, 100).unwrap();
    assert!(!second.is_empty());
    assert!(second[0].seq > after);
    // 序列单调。
    for pair in second.windows(2) {
        assert!(pair[1].seq > pair[0].seq);
    }
    // limit 0 / 101 拒绝。
    assert!(store.list_events(0, 0).is_err());
    assert!(store.list_events(0, 101).is_err());

    remove_dir(&dir);
}

#[test]
fn list_tasks_stable_cursor_pagination() {
    let dir = temp_dir("list");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    for i in 0..5 {
        let plan_id = format!("plan_l{i}");
        store
            .put_plan(relay_store::PlanInput {
                plan_id: &plan_id,
                project_id: "proj_l",
                goal: "g",
                brief: "b",
                base_oid: OID,
                created_at: T0,
                expires_at: T60,
                write_scope_json: "[]",
                rule_ids_json: "[]",
                profile_id: "",
            })
            .unwrap();
        store
            .submit(SubmitInput {
                request_id: &format!("req_l{i}"),
                plan_id: &plan_id,
                now: T0,
            })
            .unwrap();
    }
    // 第一页 2 条 + 游标。
    let page1 = store.list_tasks("proj_l", None, 2).unwrap();
    assert_eq!(page1.tasks.len(), 2);
    assert_eq!(
        page1.next_cursor.as_deref(),
        Some(page1.tasks[1].task_id.as_str())
    );
    // 同游标重查结果稳定。
    let page1_again = store
        .list_tasks("proj_l", page1.next_cursor.as_deref(), 2)
        .unwrap();
    let page1_repeat = store
        .list_tasks("proj_l", page1.next_cursor.as_deref(), 2)
        .unwrap();
    assert_eq!(page1_again.tasks.len(), page1_repeat.tasks.len());
    // 走完分页后 next_cursor 为 None，合计 5 条。
    let mut cursor = page1.next_cursor.clone();
    let mut seen: Vec<String> = page1.tasks.iter().map(|t| t.task_id.clone()).collect();
    while let Some(c) = cursor {
        let page = store.list_tasks("proj_l", Some(&c), 2).unwrap();
        seen.extend(page.tasks.iter().map(|t| t.task_id.clone()));
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 5);
    // 其他项目为空；limit 越界拒绝。
    let other = store.list_tasks("proj_other", None, 50).unwrap();
    assert!(other.tasks.is_empty());
    assert!(other.next_cursor.is_none());
    assert!(store.list_tasks("proj_l", None, 0).is_err());
    assert!(store.list_tasks("proj_l", None, 51).is_err());

    remove_dir(&dir);
}

#[test]
fn review_enforces_digests_and_state() {
    let dir = temp_dir("review");
    let store = Store::open(&dir.join("relay.db")).unwrap();
    let task_id = stage_running_task(&store, "plan_v1", "req_v1");
    verify_transition(&store, &task_id, 4);
    store
        .register_artifact_and_transition(ArtifactRegisterInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 5,
            artifact_dir: "/managed/artifacts/candidate",
            manifest_sha256: SHA,
            patch_sha256: SHA,
            receipt_sha256: SHA,
            handoff_sha256: SHA,
            now: T0,
        })
        .unwrap();

    // 摘要不符：拒绝且不推进。
    let err = store
        .review(ReviewInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 6,
            verdict: "accept",
            request_id: "0e2c1a25-4e9a-4f8e-8f9e-1b6d1d6d6f10",
            diff_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            test_sha256: SHA,
            note: "",
            now: T0,
        })
        .unwrap_err();
    assert!(
        err.to_string().contains("diff_sha256 与登记制品不符"),
        "{err:?}"
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        State::AwaitingReview
    );

    // 未登记 attempt：拒绝。
    let err = store
        .review(ReviewInput {
            task_id: &task_id,
            attempt_no: 2,
            expected_revision: 6,
            verdict: "accept",
            request_id: "0e2c1a25-4e9a-4f8e-8f9e-1b6d1d6d6f10",
            diff_sha256: SHA,
            test_sha256: SHA,
            note: "",
            now: T0,
        })
        .unwrap_err();
    assert!(err.to_string().contains("无已登记制品"), "{err:?}");

    // 摘要一致：accept → completed。
    let outcome = store
        .review(ReviewInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 6,
            verdict: "accept",
            request_id: "0e2c1a25-4e9a-4f8e-8f9e-1b6d1d6d6f10",
            diff_sha256: SHA,
            test_sha256: SHA,
            note: "manual audit note",
            now: T0,
        })
        .unwrap();
    assert_eq!(outcome.state, State::Completed);
    let saved_note: String = store
        .conn()
        .unwrap()
        .query_row(
            "SELECT reason_code FROM events WHERE task_id=?1 AND kind='review_note'",
            [task_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved_note, "manual audit note");

    // 非 awaiting_review 状态再次验收：WrongState。
    let err = store
        .review(ReviewInput {
            task_id: &task_id,
            attempt_no: 1,
            expected_revision: 7,
            verdict: "accept",
            request_id: "0e2c1a25-4e9a-4f8e-8f9e-1b6d1d6d6f11",
            diff_sha256: SHA,
            test_sha256: SHA,
            note: "",
            now: T0,
        })
        .unwrap_err();
    assert!(
        matches!(err, relay_store::StoreError::WrongState { .. }),
        "{err:?}"
    );

    remove_dir(&dir);
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}
