//! 存储层错误：错误码与 04 协议对齐，供后续协议层直接映射。

use std::fmt;

use relay_core::State;

/// 存储层错误。[`StoreError::code`] 返回 04 协议的错误码字符串。
#[derive(Debug)]
pub enum StoreError {
    /// 输入不合法（未知 plan、ID 形状、长度超限等）。
    InvalidInput(String),
    /// Task 不存在。
    NotFound { task_id: String },
    /// 计划已过期（`now >= expires_at`）且未被消费。
    PlanExpired { plan_id: String },
    /// 计划已被成功消费；按 04 携带已建 task ID（如仍在保留期）。
    PlanUsed {
        plan_id: String,
        existing_task_id: Option<String>,
    },
    /// 同一 request_id 换了语义参数；按 04 携带原 task ID。
    IdempotencyConflict {
        request_id: String,
        existing_task_id: Option<String>,
    },
    /// attempt 或 revision 与预期不符；携带当前值供调用方刷新后重试。
    StaleRevision {
        task_id: String,
        current_revision: u64,
        current_active_attempt: u32,
    },
    /// 当前状态不接受该事件（非法转换）。
    WrongState {
        task_id: String,
        state: State,
        event: relay_core::Event,
    },
    /// 当前状态不满足重试资格（含中断未确认无活动 worker，may_retry 为 false）。
    RetryNotAllowed { task_id: String, state: State },
    /// 新 plan 的持久化条件（project/goal/base_oid）与原任务不一致。
    PlanStale { plan_id: String, task_id: String },
    /// 启动代号已被占用（全局唯一）：调用方须换用新的代号重试。
    LaunchCodeConflict { launch_code: String },
    /// 数据库 schema 版本高于本构建支持的最高版本，拒绝打开（03）。
    SchemaTooNew { found: i64, supported: i64 },
    /// 数据库 schema 版本低于本构建支持值（v1）：需先执行显式离线迁移，
    /// open 不做静默升级。
    SchemaTooOld { found: i64, supported: i64 },
    /// 本机环境无法满足规格要求（WAL 不可用、元数据损坏、互斥锁中毒等）。
    Environment(String),
    /// SQLite / rusqlite 底层错误。
    Db(rusqlite::Error),
}

impl StoreError {
    /// 04 协议错误码。
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::InvalidInput(_) => "INVALID_ARGUMENT",
            StoreError::NotFound { .. } => "NOT_FOUND",
            StoreError::PlanExpired { .. } => "PLAN_EXPIRED",
            StoreError::PlanUsed { .. } => "PLAN_USED",
            StoreError::IdempotencyConflict { .. } => "IDEMPOTENCY_CONFLICT",
            StoreError::StaleRevision { .. } => "STALE_REVISION",
            StoreError::WrongState { .. } => "WRONG_STATE",
            StoreError::RetryNotAllowed { .. } => "WRONG_STATE",
            StoreError::PlanStale { .. } => "PLAN_STALE",
            StoreError::LaunchCodeConflict { .. } => "INVALID_ARGUMENT",
            StoreError::SchemaTooNew { .. } => "UNSUPPORTED_VERSION",
            StoreError::SchemaTooOld { .. } => "UNSUPPORTED_VERSION",
            StoreError::Environment(_) => "SERVICE_UNAVAILABLE",
            StoreError::Db(_) => "SQLITE_ERROR",
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::InvalidInput(message) => write!(f, "INVALID_ARGUMENT: {message}"),
            StoreError::NotFound { task_id } => write!(f, "NOT_FOUND: task {task_id} 不存在"),
            StoreError::PlanExpired { plan_id } => {
                write!(f, "PLAN_EXPIRED: plan {plan_id} 已过期且未被消费")
            }
            StoreError::PlanUsed {
                plan_id,
                existing_task_id,
            } => match existing_task_id {
                Some(task_id) => {
                    write!(f, "PLAN_USED: plan {plan_id} 已被消费，已建 task {task_id}")
                }
                None => write!(f, "PLAN_USED: plan {plan_id} 已被消费，已建 task 不在保留期"),
            },
            StoreError::IdempotencyConflict {
                request_id,
                existing_task_id,
            } => match existing_task_id {
                Some(task_id) => write!(
                    f,
                    "IDEMPOTENCY_CONFLICT: request {request_id} 已用不同参数提交过（task {task_id}）"
                ),
                None => write!(f, "IDEMPOTENCY_CONFLICT: request {request_id} 已用不同参数提交过"),
            },
            StoreError::StaleRevision {
                task_id,
                current_revision,
                current_active_attempt,
            } => write!(
                f,
                "STALE_REVISION: task {task_id} 当前 revision {current_revision}/attempt {current_active_attempt} 与预期不符"
            ),
            StoreError::WrongState { task_id, state, event } => write!(
                f,
                "WRONG_STATE: task {task_id} 状态 {state} 不接受事件 {event}"
            ),
            StoreError::RetryNotAllowed { task_id, state } => write!(
                f,
                "WRONG_STATE: task {task_id} 状态 {state} 不满足重试资格（中断需先确认无活动 worker）"
            ),
            StoreError::PlanStale { plan_id, task_id } => write!(
                f,
                "PLAN_STALE: plan {plan_id} 的持久化条件与 task {task_id} 不一致"
            ),
            StoreError::LaunchCodeConflict { launch_code } => write!(
                f,
                "INVALID_ARGUMENT: 启动代号 {launch_code} 已被占用，请换用新的全局唯一代号"
            ),
            StoreError::SchemaTooNew { found, supported } => write!(
                f,
                "UNSUPPORTED_VERSION: 数据库 schema v{found} 高于本构建支持的 v{supported}，拒绝打开"
            ),
            StoreError::SchemaTooOld { found, supported } => write!(
                f,
                "UNSUPPORTED_VERSION: 数据库 schema v{found} 低于本构建支持的 v{supported}，请先执行 migrate_v{found}_to_v{next} 离线迁移，open 不做静默升级", next = found.saturating_add(1)
            ),
            StoreError::Environment(message) => write!(f, "SERVICE_UNAVAILABLE: {message}"),
            StoreError::Db(e) => write!(f, "SQLITE_ERROR: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Db(e)
    }
}
