//! WP0 状态契约测试（独立验收基线）：
//! - 逐条合法转换（12 规格 WP0 事件表，共 25 条边）；
//! - 其余全部状态 × 事件组合一律拒绝（13 状态 × 15 事件全组合）；
//! - 全部状态的恢复分类与重试资格；
//! - 关键负例：未确认进程退出不得直达 cancelled，重复取消被拒绝。
//!
//! 本文件只使用 `relay_core` 公共 API 与标准库，作为独立验收的对照表。

use relay_core::{may_retry, recover, transition, Event, InvalidTransition, State};

/// 12 规格 WP0 事件表的全部合法边：`(当前状态, 事件, 结果状态)`。
/// 与 `spec/03-scheduler-storage.md` 状态表一致；规格冻结，改动须走规格流程。
const LEGAL_EDGES: &[(State, Event, State)] = &[
    // Claim：queued → preparing
    (State::Queued, Event::Claim, State::Preparing),
    // PreflightBlocked：queued、preparing → blocked
    (State::Queued, Event::PreflightBlocked, State::Blocked),
    (State::Preparing, Event::PreflightBlocked, State::Blocked),
    // PrepareLaunch：preparing → launching
    (State::Preparing, Event::PrepareLaunch, State::Launching),
    // PreparationFailed：preparing → failed
    (State::Preparing, Event::PreparationFailed, State::Failed),
    // Spawned / SpawnFailed：launching → running / failed
    (State::Launching, Event::Spawned, State::Running),
    (State::Launching, Event::SpawnFailed, State::Failed),
    // WorkerSucceeded / WorkerFailed：running → verifying / failed
    (State::Running, Event::WorkerSucceeded, State::Verifying),
    (State::Running, Event::WorkerFailed, State::Failed),
    // VerificationPassed / VerificationFailed：verifying → awaiting_review / failed
    (
        State::Verifying,
        Event::VerificationPassed,
        State::AwaitingReview,
    ),
    (State::Verifying, Event::VerificationFailed, State::Failed),
    // CancelRequested：queued、preparing、awaiting_review → cancelled；
    // launching、running、verifying → cancelling
    (State::Queued, Event::CancelRequested, State::Cancelled),
    (State::Preparing, Event::CancelRequested, State::Cancelled),
    (
        State::AwaitingReview,
        Event::CancelRequested,
        State::Cancelled,
    ),
    (State::Launching, Event::CancelRequested, State::Cancelling),
    (State::Running, Event::CancelRequested, State::Cancelling),
    (State::Verifying, Event::CancelRequested, State::Cancelling),
    // AllOwnedWorkersStopped：cancelling → cancelled
    (
        State::Cancelling,
        Event::AllOwnedWorkersStopped,
        State::Cancelled,
    ),
    // MarkInterrupted：preparing、launching、running、verifying、cancelling → interrupted
    (State::Preparing, Event::MarkInterrupted, State::Interrupted),
    (State::Launching, Event::MarkInterrupted, State::Interrupted),
    (State::Running, Event::MarkInterrupted, State::Interrupted),
    (State::Verifying, Event::MarkInterrupted, State::Interrupted),
    (
        State::Cancelling,
        Event::MarkInterrupted,
        State::Interrupted,
    ),
    // ReviewAccepted / ReviewRejected：awaiting_review → completed / rejected
    (
        State::AwaitingReview,
        Event::ReviewAccepted,
        State::Completed,
    ),
    (
        State::AwaitingReview,
        Event::ReviewRejected,
        State::Rejected,
    ),
];

#[test]
fn every_legal_edge_succeeds() {
    for &(from, event, to) in LEGAL_EDGES {
        assert_eq!(
            transition(from, event),
            Ok(to),
            "合法边失败: {from} + {event}"
        );
    }
}

#[test]
fn every_other_state_event_combination_is_rejected() {
    // 全组合 13 × 15 = 195 种：不在合法表内的组合必须 Err，
    // 且错误携带原始 from/event；在表内的组合必须恰好映射到表中的结果。
    for &state in State::ALL {
        for &event in Event::ALL {
            let expected = match LEGAL_EDGES
                .iter()
                .copied()
                .find(|&(f, e, _)| f == state && e == event)
            {
                Some((_, _, to)) => Ok(to),
                None => Err(InvalidTransition { from: state, event }),
            };
            assert_eq!(
                transition(state, event),
                expected,
                "组合 (state={state}, event={event}) 与契约不符"
            );
        }
    }
}

#[test]
fn contract_table_shape_matches_frozen_spec() {
    // 冻结基线的形状检查：状态 13 个、事件 15 个、合法边 25 条；
    // 每个 (from, event) 只出现一次；每个事件至少有一条合法边；
    // 有出边的状态恰好是 7 个非终态（重试建新 attempt 与恢复不走事件转换，
    // blocked/failed/cancelled/interrupted/rejected/completed 在本状态机内无出边）。
    assert_eq!(State::ALL.len(), 13);
    assert_eq!(Event::ALL.len(), 15);
    assert_eq!(LEGAL_EDGES.len(), 25);

    for (i, &(f1, e1, _)) in LEGAL_EDGES.iter().enumerate() {
        for &(f2, e2, _) in LEGAL_EDGES.iter().skip(i + 1) {
            assert_ne!((f1, e1), (f2, e2), "合法组合重复: {f1} + {e1}");
        }
    }
    for &event in Event::ALL {
        assert!(
            LEGAL_EDGES.iter().any(|&(_, e, _)| e == event),
            "事件 {event} 没有任何合法边"
        );
    }
    let states_with_outgoing = [
        State::Queued,
        State::Preparing,
        State::Launching,
        State::Running,
        State::Verifying,
        State::Cancelling,
        State::AwaitingReview,
    ];
    for &state in State::ALL {
        let has_edge = LEGAL_EDGES.iter().any(|&(f, _, _)| f == state);
        assert_eq!(
            has_edge,
            states_with_outgoing.contains(&state),
            "状态 {state} 的出边集合与冻结规格不符"
        );
    }
}

#[test]
fn worker_states_reach_cancelled_only_via_confirmed_stop() {
    // launching/running/verifying 上任何事件都不能直达 cancelled：
    // CancelRequested 只进入 cancelling，必须经 AllOwnedWorkersStopped
    // 确认全部受管 worker 停止后才标 cancelled；不能确认退出时走
    // MarkInterrupted → interrupted。
    for state in [State::Launching, State::Running, State::Verifying] {
        for &event in Event::ALL {
            assert_ne!(
                transition(state, event),
                Ok(State::Cancelled),
                "{state} + {event} 未确认退出不得直达 cancelled"
            );
        }
        assert_eq!(
            transition(state, Event::CancelRequested),
            Ok(State::Cancelling)
        );
        assert_eq!(
            transition(state, Event::MarkInterrupted),
            Ok(State::Interrupted)
        );
    }
    assert_eq!(
        transition(State::Cancelling, Event::AllOwnedWorkersStopped),
        Ok(State::Cancelled)
    );
}

#[test]
fn repeated_cancel_is_not_a_second_valid_transition() {
    // 12 规格：取消重复不属于第二个有效 transition，幂等由 WP1 实现。
    assert_eq!(
        transition(State::Cancelling, Event::CancelRequested),
        Err(InvalidTransition {
            from: State::Cancelling,
            event: Event::CancelRequested,
        })
    );
    assert_eq!(
        transition(State::Cancelled, Event::CancelRequested),
        Err(InvalidTransition {
            from: State::Cancelled,
            event: Event::CancelRequested,
        })
    );
    assert_eq!(
        transition(State::Cancelled, Event::AllOwnedWorkersStopped),
        Err(InvalidTransition {
            from: State::Cancelled,
            event: Event::AllOwnedWorkersStopped,
        })
    );
}

#[test]
fn recovery_maps_all_active_states_to_interrupted() {
    // 03 规格恢复第 3 条：preparing、launching、running、verifying、cancelling
    // 重启后一律 interrupted，不自动二次执行。
    for state in [
        State::Preparing,
        State::Launching,
        State::Running,
        State::Verifying,
        State::Cancelling,
    ] {
        assert_eq!(recover(state), State::Interrupted, "recover({state})");
    }
}

#[test]
fn recovery_keeps_all_other_states() {
    // queued 可正常处理；awaiting_review 与已终结状态保持不变
    // （含已处于 interrupted 的状态）。
    for state in [
        State::Queued,
        State::AwaitingReview,
        State::Blocked,
        State::Failed,
        State::Cancelled,
        State::Interrupted,
        State::Rejected,
        State::Completed,
    ] {
        assert_eq!(recover(state), state, "recover({state}) 不应改变状态");
    }
}

#[test]
fn retry_eligibility_covers_every_state_and_flag() {
    // 12 规格：blocked、failed、rejected、cancelled 可重试；
    // interrupted 仅在确认无活动 worker 时；其余一律 false。
    // 两种取值都验证，确认标志对非 interrupted 状态无影响。
    for &state in State::ALL {
        for confirmed in [false, true] {
            let expected = match state {
                State::Blocked | State::Failed | State::Rejected | State::Cancelled => true,
                State::Interrupted => confirmed,
                _ => false,
            };
            assert_eq!(
                may_retry(state, confirmed),
                expected,
                "may_retry({state}, {confirmed})"
            );
        }
    }
}

#[test]
fn interrupted_retry_requires_no_active_worker_confirmation() {
    // 03 规格：cancelling 无法确认退出归 interrupted，禁重试直到人工检查。
    assert!(!may_retry(State::Interrupted, false));
    assert!(may_retry(State::Interrupted, true));
    // 确认后也只是重试资格：新 attempt 由后续层创建，
    // may_retry 不改写旧 attempt 状态（签名只做纯判断）。
    for state in [
        State::Queued,
        State::Preparing,
        State::Launching,
        State::Running,
        State::Verifying,
        State::Cancelling,
        State::AwaitingReview,
        State::Completed,
    ] {
        assert!(
            !may_retry(state, true),
            "状态 {state} 即使确认无活动 worker 也不可重试"
        );
    }
}
