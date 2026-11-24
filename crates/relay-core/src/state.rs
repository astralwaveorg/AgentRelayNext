//! WP0 纯状态机：attempt 状态表、恢复分类与重试资格。
//!
//! 依据 `agent-relay-design-docs/spec/03-scheduler-storage.md` 的"唯一首版状态表"
//! 与 `spec/12-glm-pilot.md` 的 WP0 事件表实现。本模块只做纯转换：
//! 不触碰进程、磁盘、SQLite、网络；事件存库、CAS、attempt 生命周期属于 WP1。
//! 非法转换一律报错，不让后续运维行为依赖代码猜测。

use std::fmt;

/// Attempt 的可见状态（03 规格"唯一首版状态表"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// 已排队，等待领取。
    Queued,
    /// 准备中：工作副本与预检；此时不准生成模型请求。
    Preparing,
    /// 启动中：持久化派发栅栏后才尝试启动。
    Launching,
    /// 运行中：子进程回收且有有效成功终止帧后才验证。
    Running,
    /// 验证中：制品原子发布、差异合规且验证通过才允许验收。
    Verifying,
    /// 取消中：等待确认全部受管 worker 停止。
    Cancelling,
    /// 待验收：显式提交验收结果或取消。
    AwaitingReview,
    /// 仅用于发起前条件不齐；运行中报错为 failed，原因保留。
    Blocked,
    /// 失败：适配器错误、测试非零或超时；可留部分制品。
    Failed,
    /// 已取消：全部受管 worker 确认停止后才可到达。
    Cancelled,
    /// 已中断：重启或无法确认回收；未证明无残留前禁止重试。
    Interrupted,
    /// 验收被拒。
    Rejected,
    /// 已完成：保留原验收收据；改建新任务。
    Completed,
}

impl State {
    /// 全部状态，供契约测试做全组合枚举；顺序即上面文档的顺序。
    pub const ALL: &'static [State] = &[
        State::Queued,
        State::Preparing,
        State::Launching,
        State::Running,
        State::Verifying,
        State::Cancelling,
        State::AwaitingReview,
        State::Blocked,
        State::Failed,
        State::Cancelled,
        State::Interrupted,
        State::Rejected,
        State::Completed,
    ];
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            State::Queued => "queued",
            State::Preparing => "preparing",
            State::Launching => "launching",
            State::Running => "running",
            State::Verifying => "verifying",
            State::Cancelling => "cancelling",
            State::AwaitingReview => "awaiting_review",
            State::Blocked => "blocked",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
            State::Interrupted => "interrupted",
            State::Rejected => "rejected",
            State::Completed => "completed",
        };
        f.write_str(name)
    }
}

/// 作用于 attempt 状态的事件（12 规格 WP0 事件表）。
///
/// `SpawnFailed` 只接收已知没启动的证据，`CancelRequested` 到达
/// `cancelled` 前提由调用层先确认相关进程已回收；本状态机不验证证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    /// 领取：queued → preparing。
    Claim,
    /// 预检发现条件不齐：queued/preparing → blocked。
    PreflightBlocked,
    /// 预检通过，准备启动：preparing → launching。
    PrepareLaunch,
    /// 准备失败：preparing → failed。
    PreparationFailed,
    /// 已成功启动：launching → running。
    Spawned,
    /// 能证明没启动：launching → failed。
    SpawnFailed,
    /// worker 报告成功：running → verifying。
    WorkerSucceeded,
    /// worker 报告失败：running → failed。
    WorkerFailed,
    /// 验证通过：verifying → awaiting_review。
    VerificationPassed,
    /// 验证失败：verifying → failed。
    VerificationFailed,
    /// 请求取消；queued/preparing/awaiting_review → cancelled，
    /// launching/running/verifying → cancelling。重复取消不是有效转换。
    CancelRequested,
    /// 已确认全部受管 worker 停止：cancelling → cancelled。
    AllOwnedWorkersStopped,
    /// 标记中断：五个活动状态 → interrupted。
    MarkInterrupted,
    /// 验收通过：awaiting_review → completed。
    ReviewAccepted,
    /// 验收拒绝：awaiting_review → rejected。
    ReviewRejected,
}

impl Event {
    /// 全部事件，供契约测试做全组合枚举；顺序即上面文档的顺序。
    pub const ALL: &'static [Event] = &[
        Event::Claim,
        Event::PreflightBlocked,
        Event::PrepareLaunch,
        Event::PreparationFailed,
        Event::Spawned,
        Event::SpawnFailed,
        Event::WorkerSucceeded,
        Event::WorkerFailed,
        Event::VerificationPassed,
        Event::VerificationFailed,
        Event::CancelRequested,
        Event::AllOwnedWorkersStopped,
        Event::MarkInterrupted,
        Event::ReviewAccepted,
        Event::ReviewRejected,
    ];
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Event::Claim => "Claim",
            Event::PreflightBlocked => "PreflightBlocked",
            Event::PrepareLaunch => "PrepareLaunch",
            Event::PreparationFailed => "PreparationFailed",
            Event::Spawned => "Spawned",
            Event::SpawnFailed => "SpawnFailed",
            Event::WorkerSucceeded => "WorkerSucceeded",
            Event::WorkerFailed => "WorkerFailed",
            Event::VerificationPassed => "VerificationPassed",
            Event::VerificationFailed => "VerificationFailed",
            Event::CancelRequested => "CancelRequested",
            Event::AllOwnedWorkersStopped => "AllOwnedWorkersStopped",
            Event::MarkInterrupted => "MarkInterrupted",
            Event::ReviewAccepted => "ReviewAccepted",
            Event::ReviewRejected => "ReviewRejected",
        };
        f.write_str(name)
    }
}

/// 非法转换：当前状态不接受该事件。其余 (state, event) 组合一律错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    /// 事件到达时的状态。
    pub from: State,
    /// 被拒绝的事件。
    pub event: Event,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid transition: {} does not accept event {}",
            self.from, self.event
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// 状态机单步转换。合法边以 12 规格 WP0 事件表为准，其余组合返回
/// [`InvalidTransition`]，不改写当前状态。
///
/// 关键约束：
/// - `CancelRequested` 在 launching/running/verifying 只进入 `cancelling`；
///   到 `cancelled` 必须先经 `AllOwnedWorkersStopped` 确认退出。
///   重复取消不是第二个有效转换，幂等由 WP1 处理。
/// - 无法确认进程已回收时不发 `CancelRequested` / `AllOwnedWorkersStopped`，
///   改发 [`Event::MarkInterrupted`]。
/// - `SpawnFailed` 只在调用层拿到"确定没启动"的证据后使用。
pub fn transition(state: State, event: Event) -> Result<State, InvalidTransition> {
    let next = match (state, event) {
        (State::Queued, Event::Claim) => State::Preparing,
        (State::Queued, Event::PreflightBlocked) | (State::Preparing, Event::PreflightBlocked) => {
            State::Blocked
        }
        (State::Preparing, Event::PrepareLaunch) => State::Launching,
        (State::Preparing, Event::PreparationFailed) => State::Failed,
        (State::Launching, Event::Spawned) => State::Running,
        (State::Launching, Event::SpawnFailed) => State::Failed,
        (State::Running, Event::WorkerSucceeded) => State::Verifying,
        (State::Running, Event::WorkerFailed) => State::Failed,
        (State::Verifying, Event::VerificationPassed) => State::AwaitingReview,
        (State::Verifying, Event::VerificationFailed) => State::Failed,
        (State::Queued, Event::CancelRequested)
        | (State::Preparing, Event::CancelRequested)
        | (State::AwaitingReview, Event::CancelRequested) => State::Cancelled,
        (State::Launching, Event::CancelRequested)
        | (State::Running, Event::CancelRequested)
        | (State::Verifying, Event::CancelRequested) => State::Cancelling,
        (State::Cancelling, Event::AllOwnedWorkersStopped) => State::Cancelled,
        (State::Preparing, Event::MarkInterrupted)
        | (State::Launching, Event::MarkInterrupted)
        | (State::Running, Event::MarkInterrupted)
        | (State::Verifying, Event::MarkInterrupted)
        | (State::Cancelling, Event::MarkInterrupted) => State::Interrupted,
        (State::AwaitingReview, Event::ReviewAccepted) => State::Completed,
        (State::AwaitingReview, Event::ReviewRejected) => State::Rejected,
        _ => return Err(InvalidTransition { from: state, event }),
    };
    Ok(next)
}

/// 重启恢复分类（03 规格"启动栅栏与恢复"第 3 条）：
/// preparing、launching、running、verifying、cancelling 一律归入
/// `interrupted`，不自动二次执行；queued 可正常处理；awaiting_review
/// 与已终结状态保持不变。
pub fn recover(state: State) -> State {
    match state {
        State::Preparing
        | State::Launching
        | State::Running
        | State::Verifying
        | State::Cancelling => State::Interrupted,
        other => other,
    }
}

/// 重试资格判断（12 规格 WP0）：blocked、failed、rejected、cancelled
/// 可重试；interrupted 仅在 `no_active_worker_confirmed`（已确认无活动
/// worker）时为 true；其余一律 false。
///
/// 只判断资格，不把旧 attempt 写回 queued；确认资格后由后续层创建
/// 新 attempt（WP1）。中途直接返回 false 的状态即使参数为 true 也不放行。
pub fn may_retry(state: State, no_active_worker_confirmed: bool) -> bool {
    match state {
        State::Blocked | State::Failed | State::Rejected | State::Cancelled => true,
        State::Interrupted => no_active_worker_confirmed,
        _ => false,
    }
}
