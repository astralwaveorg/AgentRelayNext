//! 最小 SQLite 存储实现：schema v1、WAL/耐久配置、`relay_submit`
//! 原子事务与提交去重。
//!
//! 去重次序按 03：先查 request（同 ID 同参数返回原结果，同 ID 异参数报
//! IDEMPOTENCY_CONFLICT），再查 plan（已消费报 PLAN_USED 并带回已建 task，
//! 未消费但过期报 PLAN_EXPIRED）。时间一律由调用方传入 UTC RFC3339（仅
//! `Z` 时区、秒值 00–59，不支持闰秒）；到期判断解析为时间点后比较，
//! 小数秒在长度上限内按输入精度精确参与比较（尾零不改变时间点），整秒
//! 与小数秒混用安全。同 ID 重放返回首次提交保存的快照，实时状态由
//! [`Store::get_task`] 提供。`params_hash` 是非加密的 FNV-1a 64 位摘要，
//! 仅作索引与墓碑标识，参数一致性以 `params_canon` 精确相等判定。

#[path = "stage_run.rs"]
mod stage_run;
pub use stage_run::{migrate_v8_to_v9, StageFinish, StageKey, StageKind, StageRunRecord};

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relay_core::{may_retry, recover, transition, Event, State};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};

use crate::error::StoreError;

/// `relay_submit` 操作名（04 协议工具名）。
pub const OP_SUBMIT: &str = "relay_submit";

/// `relay_retry` 操作名（04 协议工具名）。
pub const OP_RETRY: &str = "relay_retry";
/// 取消操作去重命名空间（04：relay_cancel 幂等）。
pub const OP_CANCEL: &str = "relay_cancel";
/// 验收操作去重命名空间（04：relay_review 幂等）。
pub const OP_REVIEW: &str = "relay_review";

/// 本构建支持的 schema 版本。更高版本拒绝打开；v1 须先经
/// [`migrate_v1_to_v2`] 显式离线迁移，open 不做静默升级（03：迁移从已知
/// schema 单向执行；未知更高版本时拒绝写入）。
/// 本构建支持的 schema 版本（供运维检查入口引用，避免写死过期）。
pub const SCHEMA_VERSION: i64 = 9;

/// 迁移的来源版本。
const MIGRATE_FROM_VERSION: i64 = 1;

/// 忙等待上限：并发写序列化的等待窗口（03：忙等待上限）。
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

const SCHEMA_SQL: &str = "\
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
    status     TEXT NOT NULL CHECK (status IN ('unused', 'used')),
    write_scope  TEXT NOT NULL DEFAULT '[]',
    rule_ids     TEXT NOT NULL DEFAULT '[]',
    profile_id   TEXT NOT NULL DEFAULT ''
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

CREATE TABLE IF NOT EXISTS launch_fences (
    task_id     TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no  INTEGER NOT NULL CHECK (attempt_no > 0),
    launch_code TEXT NOT NULL UNIQUE,
    worktree_id TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS artifacts (
    task_id         TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no      INTEGER NOT NULL CHECK (attempt_no > 0),
    artifact_dir    TEXT NOT NULL,
    manifest_sha256 TEXT NOT NULL,
    patch_sha256    TEXT NOT NULL,
    receipt_sha256  TEXT NOT NULL,
    handoff_sha256  TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS dispatch_gate (
    task_id    TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    state      TEXT NOT NULL CHECK (state IN ('pending','released')),
    pgid       INTEGER NOT NULL DEFAULT 0,
    reason     TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS gate_resolutions (
    task_id    TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    evidence   TEXT NOT NULL,
    result     TEXT NOT NULL,
    resolved_by TEXT NOT NULL,
    resolved_at TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS managed_groups (
    task_id      TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no   INTEGER NOT NULL CHECK (attempt_no > 0),
    pgid         INTEGER NOT NULL,
    leader_pid   INTEGER NOT NULL,
    leader_start TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('tracking','blocking','resolved')),
    reason       TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);
";

/// 提交前登记的计划记录（WP1-A 最小字段子集；预览生成属后续包）。
#[derive(Debug, Clone, Copy)]
pub struct PlanInput<'a> {
    /// opaque 计划 ID。
    pub plan_id: &'a str,
    /// 项目 ID。
    pub project_id: &'a str,
    /// 任务目标（TaskSummary.goal 的来源）。
    pub goal: &'a str,
    /// 将交给外部的简报文本。
    /// 提交时冻结的写入范围（JSON 数组文本，04 write_scope[]）。
    pub write_scope_json: &'a str,
    /// 提交时冻结的验证规则 ID（JSON 数组文本，04 verification_rule_ids[]）。
    pub rule_ids_json: &'a str,
    /// 执行 profile 标识（M1 仅登记，不据此选路）。
    pub profile_id: &'a str,
    pub brief: &'a str,
    /// 完整小写十六进制 commit OID（04：不用可变引用）。
    pub base_oid: &'a str,
    /// 计划创建时间，UTC RFC3339（Z 时区、秒值 00–59）。
    pub created_at: &'a str,
    /// 过期时间，UTC RFC3339（Z 时区、秒值 00–59）；`now >= expires_at` 视为过期。
    pub expires_at: &'a str,
}

/// `relay_submit` 输入。
#[derive(Debug, Clone, Copy)]
pub struct SubmitInput<'a> {
    /// 全局唯一的 UUID 请求 ID（04：外部 ID opaque、区分大小写）。
    pub request_id: &'a str,
    /// 要消费的计划 ID。
    pub plan_id: &'a str,
    /// 调用方当前时间，UTC RFC3339（Z 时区、秒值 00–59）；用于到期判断与
    /// created_at/updated_at 落盘。
    pub now: &'a str,
}

/// 提交结果。`deduplicated = true` 表示同 request_id 同参数重放命中，
/// 返回的是已提交结果（04：同一成功请求可安全重送）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    pub task_id: String,
    pub state: State,
    pub revision: u64,
    pub active_attempt: u32,
    pub created_at: String,
    pub deduplicated: bool,
    /// 04 成功结果最小形态：`{"schema_version":1,"task":{...}}`。
    pub result_json: String,
}

/// TaskSummary（14 契约样本的最小子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummary {
    pub task_id: String,
    pub project_id: String,
    pub goal: String,
    pub state: State,
    pub revision: u64,
    pub active_attempt: u32,
    pub reason_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 请求记录（成功结果索引）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestResult {
    pub request_id: String,
    pub operation: String,
    pub plan_id: String,
    pub task_id: String,
    pub result_json: String,
    pub created_at: String,
}

/// 单阶段状态转换 CAS 输入（03：revision 是单调递增整数并用于变更操作的
/// compare-and-swap）。目标状态由 [`relay_core::transition`] 决定，
/// 不接受调用方指定。
#[derive(Debug, Clone, Copy)]
pub struct TransitionInput<'a> {
    /// Task ID。
    pub task_id: &'a str,
    /// 预期的 task 当前 active_attempt。
    pub expected_active_attempt: u32,
    /// 预期的 task/attempt 当前 revision。
    pub expected_revision: u64,
    /// 状态机事件；前提核验（进程证据等）由调用层完成。
    pub event: Event,
    /// 调用方当前时间，UTC RFC3339（Z 时区、秒值 00–59）；用于事件
    /// created_at 与 task updated_at。
    pub now: &'a str,
}

/// CAS 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub task_id: String,
    /// 转换后的新状态。
    pub state: State,
    /// 新 revision（旧值 + 1）；task、attempt 与事件三处共用。
    pub revision: u64,
    /// 不变的 active_attempt（单阶段转换不换 attempt）。
    pub active_attempt: u32,
    /// 追加事件的 seq。
    pub event_seq: i64,
}

/// 单连接 SQLite 存储（内部 `Mutex<Connection>`，`Send + Sync`）。
///
/// 并发提交通过 WAL + 忙等待上限 + `BEGIN IMMEDIATE` 串行化；
/// 03 的"单一写入队列"在 SQLite 写锁上成立，daemon 级队列属后续任务包。
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// 打开（或创建）数据库。顺序：会话级配置（无持久写入）→ schema 版本
    /// 门槛（高于本构建支持值时拒绝）→ 启用 WAL（持久属性）→ 补齐缺失的表
    /// （持久写入）。03：未知更高版本时拒绝写入，且拒绝必须发生在任何持久
    /// 写入之前。
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // 会话级设置，不产生持久写入。
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
        check_schema_version(&conn)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(StoreError::Environment(format!(
                "无法启用 WAL（journal_mode = {mode}），03 规格要求 WAL 持久化"
            )));
        }
        ensure_schema(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Open an existing database read-only for diagnostics. Unlike
    /// [`Store::open`], this never enables WAL or creates missing tables, so
    /// `doctor` cannot repair or mutate an instance just by inspecting it.
    pub fn open_read_only(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        check_schema_version(&conn)?;
        verify_schema(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// 测试与审计用的连接访问（加锁）。调用方不得长期持有或绕过去重直接写表。
    pub fn conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.lock_conn()
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Environment("连接互斥锁中毒（上一持有方 panic）".into()))
    }

    /// 登记一条计划记录；plan_id 已存在时报 INVALID_ARGUMENT。
    pub fn put_plan(&self, plan: PlanInput<'_>) -> Result<(), StoreError> {
        validate_id(plan.plan_id, "plan_id")?;
        validate_id(plan.project_id, "project_id")?;
        validate_oid(plan.base_oid)?;
        require_time(plan.created_at, "created_at")?;
        require_time(plan.expires_at, "expires_at")?;
        validate_text(plan.goal, 1024, "goal")?;
        validate_text(plan.brief, 16 * 1024, "brief")?;
        let result = self.lock_conn()?.execute(
            "INSERT INTO plans (plan_id, project_id, goal, brief, base_oid, created_at, expires_at, status,
                                write_scope, rule_ids, profile_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'unused', ?8, ?9, ?10)",
            params![
                plan.plan_id,
                plan.project_id,
                plan.goal,
                plan.brief,
                plan.base_oid,
                plan.created_at,
                plan.expires_at,
                plan.write_scope_json,
                plan.rule_ids_json,
                plan.profile_id
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::InvalidInput(format!(
                    "plan {} 已存在",
                    plan.plan_id
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// 原子提交：创建 task、attempt 1（queued）、首条事件、成功 request 记录
    /// 并把 plan 从 unused 改为 used，全部在同一事务内；任一步失败全部回滚
    /// （14：submit 原子变化，禁止随后单独补写"已提交"）。
    ///
    /// 去重先于到期检查（03）：同 request_id 同参数重放命中时返回首次提交
    /// 保存的快照——state/revision/active_attempt/created_at/result_json
    /// 同源于 requests 中保存的首次结果，不受任务状态推进影响；实时状态由
    /// [`Store::get_task`] 提供。同 request_id 异参数报
    /// [`StoreError::IdempotencyConflict`]。
    pub fn submit(&self, req: SubmitInput<'_>) -> Result<SubmitOutcome, StoreError> {
        validate_id(req.request_id, "request_id")?;
        validate_id(req.plan_id, "plan_id")?;
        let now = parse_rfc3339_utc(req.now).ok_or_else(|| {
            StoreError::InvalidInput(format!("now 不是有效的 UTC RFC3339 时间：{}", req.now))
        })?;
        let canon = canon_params(req.plan_id);
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match submit_in_tx(&tx, req, &canon, now) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 按 task ID 查回 TaskSummary；不存在返回 None。
    pub fn get_task(&self, task_id: &str) -> Result<Option<TaskSummary>, StoreError> {
        let row = self
            .lock_conn()?
            .query_row(
                "SELECT task_id, project_id, goal, state, revision, active_attempt,
                        reason_code, created_at, updated_at
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                task_id,
                project_id,
                goal,
                state,
                revision,
                active_attempt,
                reason_code,
                created_at,
                updated_at,
            )| {
                Ok(TaskSummary {
                    task_id,
                    project_id,
                    goal,
                    state: parse_state(&state)?,
                    revision: revision as u64,
                    active_attempt: active_attempt as u32,
                    reason_code,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    /// 按 request ID 查回成功请求记录（04：变更响应丢失时以同 ID 同参查结果）。
    pub fn get_request_result(
        &self,
        request_id: &str,
    ) -> Result<Option<RequestResult>, StoreError> {
        self.lock_conn()?
            .query_row(
                "SELECT request_id, operation, plan_id, task_id, result_json, created_at
                 FROM requests WHERE request_id = ?1",
                params![request_id],
                |r| {
                    Ok(RequestResult {
                        request_id: r.get(0)?,
                        operation: r.get(1)?,
                        plan_id: r.get(2)?,
                        task_id: r.get(3)?,
                        result_json: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 单阶段状态转换 CAS：在一个 IMMEDIATE 写事务内比较 task 的
    /// active_attempt/revision 并核对当前 attempt 行，一致时用
    /// [`relay_core::transition`] 决定新状态，更新 task 与 attempt 两行的
    /// state/revision、task 的 updated_at，并追加恰好一条事件；三处共用
    /// 新 revision（旧值 + 1）。缺失 task 报 NOT_FOUND；attempt 或 revision
    /// 不匹配报 STALE_REVISION（04）；非法转换报 WRONG_STATE；task 与
    /// attempt 状态/revision 不一致报 Environment，不静默修复。任何拒绝或
    /// 写入失败整体回滚，不留部分更新；已保存的 submit 响应快照不受影响。
    ///
    /// 进程证据核验属于调用层；本方法只做状态写入原语，不做队列扫描、
    /// 恢复、启动栅栏、重试或 attempt 生命周期变化。
    pub fn transition(&self, input: TransitionInput<'_>) -> Result<TransitionOutcome, StoreError> {
        self.transition_with_reason(input, None)
    }

    /// Commit an adapter failure and its trusted diagnostic in the same CAS
    /// transaction. Callers must translate untrusted adapter text first.
    pub fn fail_worker_with_reason(
        &self,
        input: TransitionInput<'_>,
        reason: &str,
    ) -> Result<TransitionOutcome, StoreError> {
        if input.event != Event::WorkerFailed
            || reason.is_empty()
            || reason.len() > 256
            || reason.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidInput("执行失败原因无效".into()));
        }
        self.transition_with_reason(input, Some(reason))
    }

    fn transition_with_reason(
        &self,
        input: TransitionInput<'_>,
        reason: Option<&str>,
    ) -> Result<TransitionOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        if input.expected_active_attempt == 0 || input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "expected_active_attempt 与 expected_revision 必须为正整数".into(),
            ));
        }
        if input.event == Event::PrepareLaunch {
            return Err(StoreError::InvalidInput(
                "PrepareLaunch 必须经 launch_fence 专用方法提交启动栅栏，不能经普通 transition 绕过"
                    .into(),
            ));
        }
        require_time(input.now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match transition_in_tx(&tx, input) {
            Ok(outcome) => {
                if let Some(reason) = reason {
                    let task_rows = tx.execute(
                        "UPDATE tasks SET reason_code=?1 WHERE task_id=?2",
                        params![reason, input.task_id],
                    )?;
                    let event_rows = tx.execute(
                        "UPDATE events SET reason_code=?1 WHERE seq=?2",
                        params![reason, outcome.event_seq],
                    )?;
                    if task_rows != 1 || event_rows != 1 {
                        return Err(StoreError::Environment("无法保存执行失败原因".into()));
                    }
                }
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 启动恢复（03「启动栅栏与恢复」第 3 条、08 T02）：在一个 IMMEDIATE
    /// 事务内读取全部任务及其当前 attempt，整批核对状态与 revision 一致
    /// （缺失、未知状态或不一致即报错回滚整批），用 [`relay_core::recover`]
    /// 分类：preparing/launching/running/verifying/cancelling 归入
    /// interrupted 并复用 WP1-B1 的写入路径以 [`Event::MarkInterrupted`]
    /// 写入 task、当前 attempt 和恰好一条事件，共用新 revision；其他状态
    /// 保持原样，不改 revision 或时间，不写事件。任一任务写入失败回滚
    /// 整批；已保存的 submit 快照与旧 attempt 保持原样。
    ///
    /// 恢复只记录中断，不代表确认进程已退出；不创建新 attempt，不派发、
    /// 重试、发信号或清理文件。由调用方（daemon）在取得实例独占锁后、
    /// 启动派发前显式调用；[`Store::open`] 不做恢复。返回本次发生变更的
    /// 任务及新 revision。
    pub fn recover(&self, now: &str) -> Result<Vec<RecoveryRecord>, StoreError> {
        require_time(now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match recover_in_tx(&tx, now) {
            Ok(records) => {
                tx.commit()?;
                Ok(records)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 按入队顺序原子领取下一条 queued 任务（03 Dispatch：串行 worker 以
    /// 创建顺序领取可运行项）：在一个 IMMEDIATE 事务内选出最早入队且仍为
    /// queued 的任务，复用 [`Store::transition`] 的事务内 CAS 以
    /// [`Event::Claim`] 推进到 preparing。入队顺序用首次提交事件
    /// （submitted）的 seq 排序——已持久化顺序，与调用方时间或 task_id
    /// 无关；重试入队属后续任务包。无 queued 任务返回 None，不写事件；
    /// 队首数据不一致（如缺 submitted 事件、attempt 行缺失）或写入失败
    /// 时返回错误并回滚，不跳过队首领取下一条。保留 revision 溢出与
    /// 事件行数校验。领取成功只代表 preparing，不代表进程已启动。
    ///
    /// 不做启动栅栏、实例锁、worker、进程启动或重试；预检与环境材料
    /// 核验由调用方在领取后进行。
    pub fn claim_next(&self, now: &str) -> Result<Option<TransitionOutcome>, StoreError> {
        require_time(now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match claim_next_in_tx(&tx, now) {
            Ok(result) => {
                tx.commit()?;
                Ok(result)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 持久化启动栅栏（03：prepare 完成后另事务准备 launching 和唯一启动
    /// 代号，提交后才可 spawn）：仅允许 preparing → launching（由
    /// [`relay_core::transition`] 决定，不复制状态表），在同一 IMMEDIATE
    /// 事务内写入栅栏记录（同一 attempt 至多一条、启动代号全局唯一）、
    /// 更新 task/attempt 的 state/revision 与 task.updated_at、追加恰好
    /// 一条 prepare_launch 事件，三处共用新 revision。commit 成功才返回
    /// 本次启动许可；写入失败整体回滚，不留许可。普通
    /// [`Store::transition`] 不接受 [`Event::PrepareLaunch`]，无法绕过。
    ///
    /// 同一 attempt 重送（无论代号是否相同）或重开库后查询，都不能获得
    /// 第二次许可：revision 已随首次栅栏推进，CAS 前提不成立。栅栏记录
    /// 在恢复时保留，但不作为进程已启动或已退出的证据。
    pub fn launch_fence(
        &self,
        input: LaunchFenceInput<'_>,
    ) -> Result<LaunchFenceOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        validate_id(input.launch_code, "launch_code")?;
        validate_id(input.worktree_id, "worktree_id")?;
        if input.expected_active_attempt == 0 || input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "expected_active_attempt 与 expected_revision 必须为正整数".into(),
            ));
        }
        require_time(input.now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match launch_fence_in_tx(&tx, input) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 按 task_id + attempt_no 查回已持久化的启动栅栏记录；不存在返回 None。
    /// 栅栏记录只说明曾授予启动许可，不代表进程已启动或已退出。
    pub fn get_launch_fence(
        &self,
        task_id: &str,
        attempt_no: u32,
    ) -> Result<Option<LaunchFenceRecord>, StoreError> {
        self.lock_conn()?
            .query_row(
                "SELECT task_id, attempt_no, launch_code, worktree_id, created_at
                 FROM launch_fences WHERE task_id = ?1 AND attempt_no = ?2",
                params![task_id, i64::from(attempt_no)],
                |r| {
                    Ok(LaunchFenceRecord {
                        task_id: r.get(0)?,
                        attempt_no: r.get::<_, i64>(1)? as u32,
                        launch_code: r.get(2)?,
                        worktree_id: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 显式重试（03：blocked/failed/rejected/cancelled 可重试，interrupted
    /// 仅在确认无活动 worker 后；04 relay_retry）：复用 [`relay_core::may_retry`]
    /// 判定资格，中断没有可信退出证据（no_active_worker_confirmed = false）
    /// 时拒绝且不创建 attempt。在同一 IMMEDIATE 事务内：去重最先（同
    /// request_id 同参重放原结果，异参报 IDEMPOTENCY_CONFLICT），CAS 校验
    /// 预期 revision，核对新 plan 与原任务的持久化条件（project/goal/
    /// base_oid 及首次提交 plan 的 brief）相符，attempt 编号按 u32 范围
    /// 检查（当前为 u32::MAX 时明确报错），创建新 queued attempt、推进
    /// active_attempt 与
    /// revision（旧值 + 1，跨尝试连续）、写入队事件与成功快照并消费
    /// plan。旧 attempt、栅栏和首次提交快照保持原样；任一步失败整体
    /// 回滚。已消费 plan 不能再被首次提交或重试使用；PLAN_USED 错误按 04
    /// 带回该 plan 成功请求记录中的 task_id。
    ///
    /// 权限、路线等尚未落库的校验由未来调用层完成，本方法不宣称已验证；
    /// 不做自动重试。返回值复用 [`SubmitOutcome`]（重试结果或重放快照）。
    pub fn retry(&self, input: RetryInput<'_>) -> Result<SubmitOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        validate_id(input.plan_id, "plan_id")?;
        validate_id(input.request_id, "request_id")?;
        if input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "expected_revision 必须为正整数".into(),
            ));
        }
        require_time(input.now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match retry_in_tx(&tx, input) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }
}

/// 单个提交事务的步骤：去重 → 计划状态 → 建行 → 消费 plan。
/// `now` 为调用方时间解析出的时间点。
fn submit_in_tx(
    tx: &Transaction<'_>,
    req: SubmitInput<'_>,
    canon: &str,
    now: TimePoint,
) -> Result<SubmitOutcome, StoreError> {
    // 1. 去重（先于到期检查，03）：命中即返回首次提交保存的快照——
    //    state/revision/active_attempt 解析自 result_json，created_at 取自
    //    requests 行，两者都是首次成功提交时原子写入的，不受任务状态
    //    推进影响。
    if let Some(replayed) = replay_request(tx, req.request_id, OP_SUBMIT, canon)? {
        return Ok(replayed);
    }

    // 2. 计划状态与有效期。
    let (status, expires_at, project_id, goal, base_oid) = tx
        .query_row(
            "SELECT status, expires_at, project_id, goal, base_oid
             FROM plans WHERE plan_id = ?1",
            params![req.plan_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::InvalidInput(format!("plan {} 不存在", req.plan_id)))?;
    if status == "used" {
        let existing_task_id = tx
            .query_row(
                "SELECT task_id FROM requests WHERE plan_id = ?1 LIMIT 1",
                params![req.plan_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        return Err(StoreError::PlanUsed {
            plan_id: req.plan_id.to_owned(),
            existing_task_id,
        });
    }
    // 计划有效期：按解析出的时间点比较，而非字符串比较（整秒与小数秒
    // 混用时字典序不可靠）。
    let expires = parse_rfc3339_utc(&expires_at).ok_or_else(|| {
        StoreError::Environment(format!(
            "plan {} 的 expires_at 无法解析：{expires_at}",
            req.plan_id
        ))
    })?;
    if now >= expires {
        return Err(StoreError::PlanExpired {
            plan_id: req.plan_id.to_owned(),
        });
    }

    // 3. 同一事务：task → attempt 1 → request → 首条事件 → 消费 plan。
    //    plan 置 used 放在最后：中途失败（如事件写入被注入故障拒绝）时
    //    plan 保持 unused，重送仍可用。
    let task_id = new_task_id();
    let state_text = State::Queued.to_string();
    tx.execute(
        "INSERT INTO tasks (task_id, plan_id, project_id, goal, base_oid, state, revision,
                            active_attempt, reason_code, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 1, NULL, ?7, ?7)",
        params![
            task_id,
            req.plan_id,
            project_id,
            goal,
            base_oid,
            state_text,
            req.now
        ],
    )?;
    tx.execute(
        "INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
         VALUES (?1, 1, ?2, 1, NULL, NULL, ?3)",
        params![task_id, state_text, req.now],
    )?;
    let result_json = format!(
        "{{\"schema_version\":1,\"task\":{{\"id\":\"{task_id}\",\"state\":\"{state_text}\",\"revision\":1,\"active_attempt\":1}}}}"
    );
    tx.execute(
        "INSERT INTO requests (request_id, operation, params_canon, params_hash, plan_id,
                               task_id, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            req.request_id,
            OP_SUBMIT,
            canon,
            params_hash(canon),
            req.plan_id,
            task_id,
            result_json,
            req.now
        ],
    )?;
    // 首条事件：提交建立 attempt 1（queued）这一可观察事实（03）。
    tx.execute(
        "INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
         VALUES (?1, 1, 1, 'submitted', ?2, NULL, ?3)",
        params![task_id, state_text, req.now],
    )?;
    tx.execute(
        "UPDATE plans SET status = 'used' WHERE plan_id = ?1",
        params![req.plan_id],
    )?;
    Ok(SubmitOutcome {
        task_id,
        state: State::Queued,
        revision: 1,
        active_attempt: 1,
        created_at: req.now.to_owned(),
        deduplicated: false,
        result_json,
    })
}

/// 去重重放（submit 与 retry 共用，03：去重检查排在最前）：同 ID 同操作
/// 同参时返回首次成功保存的快照（result_json 与 created_at 同源），
/// 跨操作或异参报 IDEMPOTENCY_CONFLICT。无该 request_id 时返回 None。
fn replay_request(
    tx: &Transaction<'_>,
    request_id: &str,
    operation: &str,
    canon: &str,
) -> Result<Option<SubmitOutcome>, StoreError> {
    let existing = tx
        .query_row(
            "SELECT operation, params_canon, task_id, result_json, created_at
             FROM requests WHERE request_id = ?1",
            params![request_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((op, params_canon, task_id, result_json, created_at)) = existing {
        if op == operation && params_canon == canon {
            let (state_text, revision, active_attempt) = parse_result_snapshot(&result_json)?;
            return Ok(Some(SubmitOutcome {
                task_id,
                state: parse_state(state_text)?,
                revision,
                active_attempt,
                created_at,
                deduplicated: true,
                result_json,
            }));
        }
        return Err(StoreError::IdempotencyConflict {
            request_id: request_id.to_owned(),
            existing_task_id: Some(task_id),
        });
    }
    Ok(None)
}

/// 单个转换事务的步骤：定位 task → CAS 前提 → 一致性核对 → transition →
/// 更新两行 → 追加一条事件。
fn transition_in_tx(
    tx: &Transaction<'_>,
    input: TransitionInput<'_>,
) -> Result<TransitionOutcome, StoreError> {
    let expected_revision = i64::try_from(input.expected_revision)
        .map_err(|_| StoreError::InvalidInput("expected_revision 超出可表示范围".into()))?;
    // 1. Task 必须存在。
    let (state_text, revision, active_attempt) = tx
        .query_row(
            "SELECT state, revision, active_attempt FROM tasks WHERE task_id = ?1",
            params![input.task_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            task_id: input.task_id.to_owned(),
        })?;
    // 2. CAS 前提：active_attempt 与 revision 与预期一致。
    if active_attempt != i64::from(input.expected_active_attempt) || revision != expected_revision {
        return Err(StoreError::StaleRevision {
            task_id: input.task_id.to_owned(),
            current_revision: revision as u64,
            current_active_attempt: active_attempt as u32,
        });
    }
    // 3. 当前 attempt 行存在且与 task 一致；不一致即数据损坏，失败不静默修复。
    let (attempt_state, attempt_revision) = tx
        .query_row(
            "SELECT state, revision FROM attempts WHERE task_id = ?1 AND attempt_no = ?2",
            params![input.task_id, active_attempt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Environment(format!(
                "task {} 的 attempt {active_attempt} 行缺失（数据库不一致）",
                input.task_id
            ))
        })?;
    if attempt_revision != revision || attempt_state != state_text {
        return Err(StoreError::Environment(format!(
            "attempt (state {attempt_state}, revision {attempt_revision}) 与 task (state {state_text}, revision {revision}) 不一致（数据库不一致）"
        )));
    }
    // 4. 新状态只由 relay_core::transition 决定。
    let current = parse_state(&state_text)?;
    let next = transition(current, input.event).map_err(|_| StoreError::WrongState {
        task_id: input.task_id.to_owned(),
        state: current,
        event: input.event,
    })?;
    let next_text = next.to_string();
    // 5. 复用 WP1-B1 的事务内写入路径（溢出检查、两行更新、事件行数校验）。
    let (new_revision, event_seq) = write_state_change(
        tx,
        input.task_id,
        &next_text,
        revision,
        active_attempt,
        input.event,
        input.now,
    )?;
    Ok(TransitionOutcome {
        task_id: input.task_id.to_owned(),
        state: next,
        revision: new_revision as u64,
        active_attempt: active_attempt as u32,
        event_seq,
    })
}

/// 事务内状态写入路径（WP1-B1 的 transition 与 WP1-B2 的 recover 共用）：
/// revision 溢出检查、更新 task 与 attempt 两行的 state/revision 及
/// task.updated_at、追加恰好一条事件，三处共用新 revision（旧值 + 1）。
/// 行数必须恰好为 1（BEFORE INSERT 触发器 RAISE(IGNORE) 等情况会静默跳过
/// 插入）。返回 (新 revision, 事件 seq)。
fn write_state_change(
    tx: &Transaction<'_>,
    task_id: &str,
    next_state: &str,
    revision: i64,
    active_attempt: i64,
    event: Event,
    now: &str,
) -> Result<(i64, i64), StoreError> {
    let new_revision = revision.checked_add(1).ok_or_else(|| {
        StoreError::Environment(format!(
            "task {task_id} 的 revision {revision} 已达 i64 上限，无法继续递增"
        ))
    })?;
    let updated = tx.execute(
        "UPDATE attempts SET state = ?1, revision = ?2 WHERE task_id = ?3 AND attempt_no = ?4",
        params![next_state, new_revision, task_id, active_attempt],
    )?;
    if updated != 1 {
        return Err(StoreError::Environment(format!(
            "attempt 行更新影响 {updated} 行（预期 1），事务将回滚"
        )));
    }
    let updated = tx.execute(
        "UPDATE tasks SET state = ?1, revision = ?2, updated_at = ?3 WHERE task_id = ?4",
        params![next_state, new_revision, now, task_id],
    )?;
    if updated != 1 {
        return Err(StoreError::Environment(format!(
            "task 行更新影响 {updated} 行（预期 1），事务将回滚"
        )));
    }
    let inserted = tx.execute(
        "INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
        params![
            task_id,
            active_attempt,
            new_revision,
            event_kind(event),
            next_state,
            now
        ],
    )?;
    if inserted != 1 {
        return Err(StoreError::Environment(format!(
            "事件写入影响 {inserted} 行（预期 1），事务将回滚"
        )));
    }
    stage_run::close_active_stage(tx, task_id, active_attempt, next_state, now)?;
    Ok((new_revision, tx.last_insert_rowid()))
}

/// 单个恢复任务的记录：本次恢复归入 interrupted 的任务及其新 revision。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub task_id: String,
    /// 恢复后的状态（恒为 interrupted）。
    pub state: State,
    /// 新 revision（旧值 + 1）。
    pub revision: u64,
    pub active_attempt: u32,
    /// 追加事件的 seq。
    pub event_seq: i64,
}

/// 启动栅栏输入（03：持久化 launching、attempt ID、启动代号和 worktree；
/// 同一 attempt 只有一次调用 spawn 的许可）。
#[derive(Debug, Clone, Copy)]
pub struct LaunchFenceInput<'a> {
    /// Task ID。
    pub task_id: &'a str,
    /// 预期的 task 当前 active_attempt。
    pub expected_active_attempt: u32,
    /// 预期的 task/attempt 当前 revision。
    pub expected_revision: u64,
    /// 全局唯一的启动代号；同一 attempt 重送或换代号重送都不能获得第二次许可。
    pub launch_code: &'a str,
    /// 本次启动绑定的 worktree 标识。
    pub worktree_id: &'a str,
    /// 调用方当前时间，UTC RFC3339（Z 时区、秒值 00–59）。
    pub now: &'a str,
}

/// 启动栅栏结果：commit 成功才是本次启动许可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFenceOutcome {
    pub task_id: String,
    /// 转换后的新状态（launching）。
    pub state: State,
    /// 新 revision（旧值 + 1）。
    pub revision: u64,
    pub active_attempt: u32,
    pub launch_code: String,
    pub worktree_id: String,
    /// 追加事件的 seq。
    pub event_seq: i64,
}

/// 已持久化的启动栅栏记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFenceRecord {
    pub task_id: String,
    pub attempt_no: u32,
    pub launch_code: String,
    pub worktree_id: String,
    pub created_at: String,
}

/// 显式重试输入（03：blocked/failed/rejected/cancelled 可重试，
/// interrupted 仅在确认无活动 worker 后；04 relay_retry）。
#[derive(Debug, Clone, Copy)]
pub struct RetryInput<'a> {
    /// Task ID。
    pub task_id: &'a str,
    /// 重试用的新计划 ID：尚未消费、未过期，且持久化条件（project/goal/
    /// base_oid）须与原任务一致。权限、路线等未落库校验由未来调用层负责。
    pub plan_id: &'a str,
    /// 新的全局唯一 request_id：同 ID 同参重放原结果，异参报冲突。
    pub request_id: &'a str,
    /// 预期的 task 当前 revision。
    pub expected_revision: u64,
    /// 已确认无活动 worker；中断任务重试的前提（复用 may_retry）。
    pub no_active_worker_confirmed: bool,
    /// 调用方当前时间，UTC RFC3339（Z 时区、秒值 00–59）。
    pub now: &'a str,
}

/// 单次恢复事务：先整批核对 task 与当前 attempt 的一致性（缺失、未知状态
/// 或两行不一致即报错回滚整批），再用 [`relay_core::recover`] 分类，仅对
/// 活动状态逐项复用 [`write_state_change`] 写入 MarkInterrupted。
/// [`Store::commit_spawn_not_started`] 的事务体：状态推进（含事件）、
/// 解封待确认标记、写审计，三处同事务。
fn commit_spawn_not_started_in_tx(
    tx: &Transaction<'_>,
    task_id: &str,
    attempt_no: u32,
    expected_revision: u64,
    evidence: &str,
    now: &str,
) -> Result<TransitionOutcome, StoreError> {
    // 1. launching → failed（SpawnFailed），事件由共享写入路径追加。
    let outcome = transition_in_tx(
        tx,
        TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision,
            event: Event::SpawnFailed,
            now,
        },
    )?;
    // 2. 解封待确认标记：必须恰好一条 pending（栅栏事务写入）。
    let released = tx.execute(
        "UPDATE dispatch_gate SET state = 'released', reason = ?1, pgid = 0, updated_at = ?2
         WHERE task_id = ?3 AND attempt_no = ?4 AND state = 'pending'",
        params![evidence, now, task_id, i64::from(attempt_no)],
    )?;
    if released != 1 {
        return Err(StoreError::Environment(format!(
            "attempt {task_id}#{attempt_no} 的待确认标记不是恰好一条 pending（数据库不一致），整体回滚"
        )));
    }
    // 3. 审计：attempt、证据、结果、操作者、时间。
    tx.execute(
        "INSERT INTO gate_resolutions (task_id, attempt_no, evidence, result, resolved_by, resolved_at)
         VALUES (?1, ?2, ?3, 'never-started', 'daemon', ?4)",
        params![task_id, i64::from(attempt_no), evidence, now],
    )?;
    Ok(outcome)
}

fn confirm_spawn_in_tx(
    tx: &Transaction<'_>,
    task_id: &str,
    attempt_no: u32,
    expected_revision: u64,
    now: &str,
) -> Result<TransitionOutcome, StoreError> {
    // Spawned 推进与组转 tracking 同一事务：任一失败整体回滚（组保持
    // pending，仍阻断领取——不依赖 spawn 后补写 blocking 记录兜底）。
    let outcome = transition_in_tx(
        tx,
        TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision,
            event: Event::Spawned,
            now,
        },
    )?;
    let n = tx.execute(
        "UPDATE managed_groups SET state = 'tracking', updated_at = ?1
         WHERE task_id = ?2 AND attempt_no = ?3 AND state = 'tracking'",
        params![now, task_id, i64::from(attempt_no)],
    )?;
    if n != 1 {
        return Err(StoreError::Environment(format!(
            "attempt {task_id}#{attempt_no} 无已登记组行可确认（数据库不一致）"
        )));
    }
    Ok(outcome)
}

fn recover_mark_groups_blocking(tx: &Transaction<'_>, now: &str) -> Result<usize, StoreError> {
    // 重启恢复：被归入 interrupted 的任务若仍持有 tracking 组（spawn
    // 已发生、退出证据未知），转为 blocking——重启后继续阻断新领取。
    // tracking → blocking（已 spawn、退出证据未知）；gate 标记保持
    // pending（spawn 未确认/退出不明）——均继续阻断，不自动解封。
    let rows = tx.execute(
        "UPDATE managed_groups SET state = 'blocking',
                reason = '重启恢复：退出证据未知，等待组退出或人工确认',
                updated_at = ?1
         WHERE state IN ('tracking')
           AND task_id IN (SELECT task_id FROM tasks WHERE state = 'interrupted')",
        params![now],
    )?;
    let _ = tx.execute(
        "UPDATE dispatch_gate SET reason = '重启恢复：spawn 或退出未确认，等待可信证据',
                updated_at = ?1
         WHERE state = 'pending'
           AND task_id IN (SELECT task_id FROM tasks WHERE state = 'interrupted')",
        params![now],
    )?;
    Ok(rows)
}

fn recover_in_tx(tx: &Transaction<'_>, now: &str) -> Result<Vec<RecoveryRecord>, StoreError> {
    let rows: Vec<(String, String, i64, i64)> = tx
        .prepare("SELECT task_id, state, revision, active_attempt FROM tasks ORDER BY rowid")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    // 1. 批内一致性核对：状态未知、attempt 行缺失或两行不一致 → 整批拒绝。
    let mut verified: Vec<(String, State, i64, i64)> = Vec::with_capacity(rows.len());
    for (task_id, state_text, revision, active_attempt) in rows {
        let current = parse_state(&state_text)?;
        let (attempt_state, attempt_revision) = tx
            .query_row(
                "SELECT state, revision FROM attempts WHERE task_id = ?1 AND attempt_no = ?2",
                params![task_id, active_attempt],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Environment(format!(
                    "task {task_id} 的 attempt {active_attempt} 行缺失（数据库不一致）"
                ))
            })?;
        if attempt_revision != revision || attempt_state != state_text {
            return Err(StoreError::Environment(format!(
                "attempt (state {attempt_state}, revision {attempt_revision}) 与 task (state {state_text}, revision {revision}) 不一致（数据库不一致）"
            )));
        }
        verified.push((task_id, current, revision, active_attempt));
    }
    // 2. 分类与写入：仅 recover 归为 interrupted 的活动状态产生变更；
    //    其他状态保持原样，不改 revision 或时间，不写事件。
    let mut records = Vec::new();
    for (task_id, current, revision, active_attempt) in verified {
        let next = recover(current);
        if next == current {
            continue;
        }
        let (new_revision, event_seq) = write_state_change(
            tx,
            &task_id,
            &next.to_string(),
            revision,
            active_attempt,
            Event::MarkInterrupted,
            now,
        )?;
        records.push(RecoveryRecord {
            task_id,
            state: next,
            revision: new_revision as u64,
            active_attempt: active_attempt as u32,
            event_seq,
        });
    }
    // 3. 中断任务的受管组转 blocking：重启后继续阻断新领取，直到获得
    //    可信退出证据（组为空）或人工确认。
    recover_mark_groups_blocking(tx, now)?;
    Ok(records)
}

/// 单次领取事务：按当前 attempt 的入队事件（submitted）seq 选出最早的
/// queued 任务（SQLite 升序排序中 NULL 在最前，缺当前 attempt 入队事件的
/// 损坏队首会被先选中并拒绝，不会跳过），再复用 [`transition_in_tx`] 的
/// CAS 推进到 preparing。重试产生的新 attempt 带新的入队事件（seq 更大），
/// 因此重试排在已经入队的任务后面，不沿用首次提交的位置。
fn claim_next_in_tx(
    tx: &Transaction<'_>,
    now: &str,
) -> Result<Option<TransitionOutcome>, StoreError> {
    // 实例级派发门（03/T08）：存在未确认退出的受管进程组时，整个实例
    // 暂停领取——任何 queued 任务都不产生 claim/栅栏/启动记录。解除
    // 只能由可信退出证据（组为空）经 resolve_managed_group 完成。
    let blocking: i64 = tx.query_row(
        "SELECT (SELECT COUNT(*) FROM dispatch_gate WHERE state = 'pending')
              + (SELECT COUNT(*) FROM managed_groups WHERE state = 'blocking')",
        [],
        |r| r.get(0),
    )?;
    if blocking > 0 {
        return Ok(None);
    }
    let candidate = tx
        .query_row(
            "SELECT t.task_id, t.state, t.revision, t.active_attempt, e.seq
             FROM tasks t
             LEFT JOIN events e
               ON e.task_id = t.task_id AND e.kind = 'submitted'
              AND e.attempt_no = t.active_attempt
             WHERE t.state = 'queued'
             ORDER BY e.seq
             LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((task_id, _, revision, active_attempt, submitted_seq)) = candidate else {
        return Ok(None);
    };
    if submitted_seq.is_none() {
        return Err(StoreError::Environment(format!(
            "queued 任务 {task_id} 缺少 submitted 事件（数据库不一致）"
        )));
    }
    // 复用 WP1-B1 的事务内 CAS：候选值取自同一事务内的读取，
    // CAS 前提与一致性核对仍由 transition_in_tx 执行。
    let input = TransitionInput {
        task_id: &task_id,
        expected_active_attempt: active_attempt as u32,
        expected_revision: revision as u64,
        event: Event::Claim,
        now,
    };
    transition_in_tx(tx, input).map(Some)
}

/// 单次重试事务：去重最先（同 ID 同参重放，即使 plan 已消费/过期、
/// revision 已推进）→ CAS 前提 → 重试资格（复用 may_retry）→ 新 plan
/// 状态与条件核对 → 同一事务内创建新 queued attempt、推进当前指针与
/// revision、写入队事件与成功快照、消费 plan。旧 attempt、栅栏和首提
/// 快照不变；任一步失败整体回滚。
fn retry_in_tx(tx: &Transaction<'_>, input: RetryInput<'_>) -> Result<SubmitOutcome, StoreError> {
    let expected_revision = i64::try_from(input.expected_revision)
        .map_err(|_| StoreError::InvalidInput("expected_revision 超出可表示范围".into()))?;
    let canon = canon_retry_params(input.task_id, input.expected_revision, input.plan_id);
    // 1. 去重最先：同 ID 同参重放原结果，跨操作或异参报冲突。
    if let Some(replayed) = replay_request(tx, input.request_id, OP_RETRY, &canon)? {
        return Ok(replayed);
    }
    // 2. Task 定位（带持久化条件列与首提 plan 引用，供新 plan 条件核对）。
    let (state_text, revision, active_attempt, task_plan_id, task_project, task_goal, task_base) =
        tx.query_row(
            "SELECT state, revision, active_attempt, plan_id, project_id, goal, base_oid
             FROM tasks WHERE task_id = ?1",
            params![input.task_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            task_id: input.task_id.to_owned(),
        })?;
    // attempt 编号按 u32 范围检查：当前须可表示，下一个（当前 + 1）不得超过
    // u32::MAX；超限时返回明确存储错误，不创建 attempt、不消费 plan、
    // 不写请求或事件。
    if active_attempt < 0 || active_attempt > i64::from(u32::MAX) {
        return Err(StoreError::Environment(format!(
            "task {} 的 attempt 编号 {active_attempt} 超出 u32 范围（数据库不一致）",
            input.task_id
        )));
    }
    if active_attempt >= i64::from(u32::MAX) {
        return Err(StoreError::Environment(format!(
            "task {} 的 attempt 编号 {active_attempt} 已达 u32 上限，无法创建下一个 attempt",
            input.task_id
        )));
    }
    // 3. CAS 前提：revision 与预期一致。
    if revision != expected_revision {
        return Err(StoreError::StaleRevision {
            task_id: input.task_id.to_owned(),
            current_revision: revision as u64,
            current_active_attempt: active_attempt as u32,
        });
    }
    // 4. 重试资格：复用 relay_core::may_retry；中断未确认无活动 worker
    //    时拒绝且不创建 attempt。
    let current = parse_state(&state_text)?;
    if !may_retry(current, input.no_active_worker_confirmed) {
        return Err(StoreError::RetryNotAllowed {
            task_id: input.task_id.to_owned(),
            state: current,
        });
    }
    // 5. 新 plan：存在、unused、未过期。PLAN_USED 时若该 plan 的成功请求
    //    记录仍在，带回记录中的 task_id（04：已用 plan 返回已建 task ID）。
    let (
        plan_status,
        plan_expires_at,
        plan_project,
        plan_goal,
        plan_base,
        plan_brief,
        plan_scope,
        plan_rules,
        plan_profile,
    ) = tx
        .query_row(
            "SELECT status, expires_at, project_id, goal, base_oid, brief,
                    write_scope, rule_ids, profile_id
             FROM plans WHERE plan_id = ?1",
            params![input.plan_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::InvalidInput(format!("plan {} 不存在", input.plan_id)))?;
    if plan_status == "used" {
        let existing_task_id = tx
            .query_row(
                "SELECT task_id FROM requests WHERE plan_id = ?1 LIMIT 1",
                params![input.plan_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        return Err(StoreError::PlanUsed {
            plan_id: input.plan_id.to_owned(),
            existing_task_id,
        });
    }
    let expires = parse_rfc3339_utc(&plan_expires_at).ok_or_else(|| {
        StoreError::Environment(format!(
            "plan {} 的 expires_at 无法解析：{plan_expires_at}",
            input.plan_id
        ))
    })?;
    let now_tp = parse_rfc3339_utc(input.now).ok_or_else(|| {
        StoreError::InvalidInput(format!("now 不是有效的 UTC RFC3339 时间：{}", input.now))
    })?;
    if now_tp >= expires {
        return Err(StoreError::PlanExpired {
            plan_id: input.plan_id.to_owned(),
        });
    }
    // 6. 新 plan 的持久化条件与原任务一致（03：用原项目、base、简报、
    //    范围和路线）；brief/scope/rules/profile 落在首次提交的 plan 行上，
    //    按 task.plan_id 回读比对。权限、路线等未落库校验由调用层负责。
    let (original_brief, original_scope, original_rules, original_profile): (
        String,
        String,
        String,
        String,
    ) = tx
        .query_row(
            "SELECT brief, write_scope, rule_ids, profile_id
             FROM plans WHERE plan_id = ?1",
            params![task_plan_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Environment(format!(
                "task {} 的首提 plan {task_plan_id} 行缺失（数据库不一致）",
                input.task_id
            ))
        })?;
    if plan_project != task_project
        || plan_goal != task_goal
        || plan_base != task_base
        || plan_brief != original_brief
        || plan_profile != original_profile
        || !same_string_set(&plan_scope, &original_scope, "write_scope")?
        || !same_string_set(&plan_rules, &original_rules, "rule_ids")?
    {
        return Err(StoreError::PlanStale {
            plan_id: input.plan_id.to_owned(),
            task_id: input.task_id.to_owned(),
        });
    }
    // 7. 当前 attempt 行一致性核对；不一致即数据损坏，不静默修复。
    let (attempt_state, attempt_revision) = tx
        .query_row(
            "SELECT state, revision FROM attempts WHERE task_id = ?1 AND attempt_no = ?2",
            params![input.task_id, active_attempt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Environment(format!(
                "task {} 的 attempt {active_attempt} 行缺失（数据库不一致）",
                input.task_id
            ))
        })?;
    if attempt_revision != revision || attempt_state != state_text {
        return Err(StoreError::Environment(format!(
            "attempt (state {attempt_state}, revision {attempt_revision}) 与 task (state {state_text}, revision {revision}) 不一致（数据库不一致）"
        )));
    }
    // 8. 同一事务写入：新 queued attempt、当前指针与 revision、入队事件、
    //    成功快照、消费 plan；revision 跨尝试连续（旧值 + 1）。
    let new_revision = revision.checked_add(1).ok_or_else(|| {
        StoreError::Environment(format!(
            "task {} 的 revision {revision} 已达 i64 上限，无法继续递增",
            input.task_id
        ))
    })?;
    let new_attempt_no = active_attempt.checked_add(1).ok_or_else(|| {
        StoreError::Environment(format!(
            "task {task_id} 的 attempt 编号已达 i64 上限",
            task_id = input.task_id
        ))
    })?;
    let state_text_new = State::Queued.to_string();
    let inserted = tx.execute(
        "INSERT INTO attempts (task_id, attempt_no, state, revision, started_at, ended_at, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
        params![input.task_id, new_attempt_no, state_text_new, new_revision, input.now],
    )?;
    if inserted != 1 {
        return Err(StoreError::Environment(format!(
            "新 attempt 写入影响 {inserted} 行（预期 1），事务将回滚"
        )));
    }
    let updated = tx.execute(
        "UPDATE tasks SET state = ?1, revision = ?2, active_attempt = ?3, updated_at = ?4, reason_code = NULL
         WHERE task_id = ?5",
        params![
            state_text_new,
            new_revision,
            new_attempt_no,
            input.now,
            input.task_id
        ],
    )?;
    if updated != 1 {
        return Err(StoreError::Environment(format!(
            "task 行更新影响 {updated} 行（预期 1），事务将回滚"
        )));
    }
    let result_json = format!(
        "{{\"schema_version\":1,\"task\":{{\"id\":\"{task_id}\",\"state\":\"{state_text_new}\",\"revision\":{new_revision},\"active_attempt\":{new_attempt_no}}}}}",
        task_id = input.task_id
    );
    let inserted = tx.execute(
        "INSERT INTO requests (request_id, operation, params_canon, params_hash, plan_id,
                               task_id, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.request_id,
            OP_RETRY,
            canon,
            params_hash(&canon),
            input.plan_id,
            input.task_id,
            result_json,
            input.now
        ],
    )?;
    if inserted != 1 {
        return Err(StoreError::Environment(format!(
            "重试快照写入影响 {inserted} 行（预期 1），事务将回滚"
        )));
    }
    // 入队事件：kind 与首次提交一致（submitted），挂在新的 attempt 上；
    // D2 的领取顺序按当前 attempt 的入队事件 seq 排序。
    let inserted = tx.execute(
        "INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
         VALUES (?1, ?2, ?3, 'submitted', ?4, NULL, ?5)",
        params![
            input.task_id,
            new_attempt_no,
            new_revision,
            state_text_new,
            input.now
        ],
    )?;
    if inserted != 1 {
        return Err(StoreError::Environment(format!(
            "入队事件写入影响 {inserted} 行（预期 1），事务将回滚"
        )));
    }
    let updated = tx.execute(
        "UPDATE plans SET status = 'used' WHERE plan_id = ?1",
        params![input.plan_id],
    )?;
    if updated != 1 {
        return Err(StoreError::Environment(format!(
            "plan 消费更新影响 {updated} 行（预期 1），事务将回滚"
        )));
    }
    Ok(SubmitOutcome {
        task_id: input.task_id.to_owned(),
        state: State::Queued,
        revision: new_revision as u64,
        active_attempt: new_attempt_no as u32,
        created_at: input.now.to_owned(),
        deduplicated: false,
        result_json,
    })
}

/// 单个栅栏事务：定位 task → CAS 前提 → attempt 一致性 → transition →
/// 栅栏记录 → 状态写入与事件。
fn launch_fence_in_tx(
    tx: &Transaction<'_>,
    input: LaunchFenceInput<'_>,
) -> Result<LaunchFenceOutcome, StoreError> {
    let expected_revision = i64::try_from(input.expected_revision)
        .map_err(|_| StoreError::InvalidInput("expected_revision 超出可表示范围".into()))?;
    // 1. Task 必须存在。
    let (state_text, revision, active_attempt) = tx
        .query_row(
            "SELECT state, revision, active_attempt FROM tasks WHERE task_id = ?1",
            params![input.task_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            task_id: input.task_id.to_owned(),
        })?;
    // 2. CAS 前提：active_attempt 与 revision 与预期一致；首次栅栏后
    //    revision 已推进，同一 attempt 的任何重送都止步于此。
    if active_attempt != i64::from(input.expected_active_attempt) || revision != expected_revision {
        return Err(StoreError::StaleRevision {
            task_id: input.task_id.to_owned(),
            current_revision: revision as u64,
            current_active_attempt: active_attempt as u32,
        });
    }
    // 3. 当前 attempt 行存在且与 task 一致；不一致即数据损坏，不静默修复。
    let (attempt_state, attempt_revision) = tx
        .query_row(
            "SELECT state, revision FROM attempts WHERE task_id = ?1 AND attempt_no = ?2",
            params![input.task_id, active_attempt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Environment(format!(
                "task {} 的 attempt {active_attempt} 行缺失（数据库不一致）",
                input.task_id
            ))
        })?;
    if attempt_revision != revision || attempt_state != state_text {
        return Err(StoreError::Environment(format!(
            "attempt (state {attempt_state}, revision {attempt_revision}) 与 task (state {state_text}, revision {revision}) 不一致（数据库不一致）"
        )));
    }
    // 4. 仅 preparing → launching：新状态由 relay_core::transition 决定。
    let current = parse_state(&state_text)?;
    let next = transition(current, Event::PrepareLaunch).map_err(|_| StoreError::WrongState {
        task_id: input.task_id.to_owned(),
        state: current,
        event: Event::PrepareLaunch,
    })?;
    let next_text = next.to_string();
    // 5. 栅栏记录先写：同一 attempt 唯一（主键）、启动代号全局唯一（UNIQUE）；
    //    冲突时区分代号占用与重复栅栏，事务随后整体回滚。
    let inserted = tx.execute(
        "INSERT INTO launch_fences (task_id, attempt_no, launch_code, worktree_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            input.task_id,
            active_attempt,
            input.launch_code,
            input.worktree_id,
            input.now
        ],
    );
    match inserted {
        Ok(1) => {}
        Ok(n) => {
            return Err(StoreError::Environment(format!(
                "栅栏写入影响 {n} 行（预期 1），事务将回滚"
            )))
        }
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let code_taken = tx
                .query_row(
                    "SELECT 1 FROM launch_fences WHERE launch_code = ?1",
                    params![input.launch_code],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_some();
            if code_taken {
                return Err(StoreError::LaunchCodeConflict {
                    launch_code: input.launch_code.to_owned(),
                });
            }
            return Err(StoreError::Environment(format!(
                "task {} 的 attempt {active_attempt} 已存在栅栏记录（数据库不一致）",
                input.task_id
            )));
        }
        Err(e) => return Err(e.into()),
    }
    // 6. 状态写入与事件：复用共享写入路径（溢出与行数校验保留）。
    let (new_revision, event_seq) = write_state_change(
        tx,
        input.task_id,
        &next_text,
        revision,
        active_attempt,
        Event::PrepareLaunch,
        input.now,
    )?;
    // 7. 实例级待确认标记与启动许可同一事务：spawn 之前持久化 pending
    //    gate 行；本事务失败则整体回滚、不启动（且不产生任何派发）。
    tx.execute(
        "INSERT INTO dispatch_gate (task_id, attempt_no, state, reason, created_at, updated_at)
         VALUES (?1, ?2, 'pending', '启动许可已授予，spawn 未确认', ?3, ?3)",
        params![input.task_id, active_attempt, input.now],
    )?;
    Ok(LaunchFenceOutcome {
        task_id: input.task_id.to_owned(),
        state: next,
        revision: new_revision as u64,
        active_attempt: active_attempt as u32,
        launch_code: input.launch_code.to_owned(),
        worktree_id: input.worktree_id.to_owned(),
        event_seq,
    })
}

/// [`Event`] → 事件流 kind（变体名的 snake_case，如 CancelRequested →
/// cancel_requested），与 WP1-A 首条事件 'submitted' 同为小写蛇形。
fn event_kind(event: Event) -> String {
    let name = event.to_string();
    let mut kind = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                kind.push('_');
            }
            kind.push(ch.to_ascii_lowercase());
        } else {
            kind.push(ch);
        }
    }
    kind
}

/// 版本门槛（只读）：meta 存在且版本高于本构建支持值时拒绝，低于支持值
/// （v1）时报错提示先离线迁移。必须发生在任何持久写入（journal_mode 切换、
/// 建表）之前。
fn check_schema_version(conn: &Connection) -> Result<(), StoreError> {
    match read_schema_version(conn)? {
        None => Ok(()),
        Some(found) if found > SCHEMA_VERSION => Err(StoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        }),
        Some(found) if found < SCHEMA_VERSION => Err(StoreError::SchemaTooOld {
            found,
            supported: SCHEMA_VERSION,
        }),
        Some(_) => Ok(()),
    }
}

/// 读取已登记的 schema 版本；meta 表不存在返回 None（全新库）。
fn read_schema_version(conn: &Connection) -> Result<Option<i64>, StoreError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !has_meta {
        return Ok(None);
    }
    let text: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Environment("meta 中缺少 schema_version".into()))?;
    let found: i64 = text
        .parse()
        .map_err(|_| StoreError::Environment(format!("schema_version 无法解析：{text}")))?;
    Ok(Some(found))
}

/// v1 → v2 的迁移 DDL；与版本号更新在同一事务内提交。
const MIGRATION_V1_TO_V2_SQL: &str = "\
CREATE TABLE IF NOT EXISTS launch_fences (
    task_id     TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no  INTEGER NOT NULL CHECK (attempt_no > 0),
    launch_code TEXT NOT NULL UNIQUE,
    worktree_id TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);
";

/// 核对迁移涉及的关键表行数，供备份一致性验证。
fn table_row_counts(conn: &Connection) -> Result<Vec<(&'static str, i64)>, StoreError> {
    let tables = ["plans", "tasks", "attempts", "events", "requests", "meta"];
    let mut counts = Vec::with_capacity(tables.len());
    for table in tables {
        let count: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        counts.push((table, count));
    }
    Ok(counts)
}

/// 将 v1 数据库显式离线迁移到 v2（03：迁移从已知 schema 单向执行；09：
/// 备份用一致 SQLite 快照，不可单复制主数据库文件忽略 WAL）。
///
/// 步骤：核验源库恰为 v1（更高版本拒绝、v2 报无需迁移）→ 用 `VACUUM INTO`
/// 产出包含 WAL 已提交内容的一致单文件备份（目标路径已存在则拒绝，不覆盖
/// 已有备份）→ 重开备份核对版本与关键表行数一致 → 在同一事务内执行迁移
/// DDL 并更新版本号。备份失败或校验失败时不改源库；迁移中途失败由事务
/// 回滚，源库保持 v1。
pub fn migrate_v1_to_v2(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    // 不带 CREATE 标志打开：源库不存在时报错返回，不创建源文件或
    // SQLite 侧车文件。
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // 会话级设置，不产生持久写入；不改动 journal_mode。
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(1) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(2) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v2，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v1，无法迁移"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v1 数据库".into(),
            ))
        }
    }
    // 一致性备份：VACUUM INTO 读取源库当前已提交内容（含 WAL），输出单文件。
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    // 校验备份：可读、仍为 v1、关键表行数与源一致。
    let source_counts = table_row_counts(&conn)?;
    let backup_conn = Connection::open(backup_path)?;
    if read_schema_version(&backup_conn)? != Some(MIGRATE_FROM_VERSION) {
        return Err(StoreError::Environment(format!(
            "备份 {} 版本异常，迁移中止",
            backup_path.display()
        )));
    }
    let backup_counts = table_row_counts(&backup_conn)?;
    drop(backup_conn);
    if backup_counts != source_counts {
        return Err(StoreError::Environment(
            "备份与源库关键表行数不一致，迁移中止".into(),
        ));
    }
    // 同一事务：迁移 DDL + 版本号；任一步失败整体回滚，源库保持 v1。
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(MIGRATION_V1_TO_V2_SQL)?;
    // 提交前必须确认版本更新恰好影响一行：BEFORE UPDATE 触发器
    // RAISE(IGNORE) 等情况会静默跳过更新，不校验就会带着 v1 版本号
    // "成功"返回。
    let updated = tx.execute(
        "UPDATE meta SET value = '2' WHERE key = 'schema_version'",
        [],
    )?;
    if updated != 1 {
        return Err(StoreError::Environment(format!(
            "版本更新影响 {updated} 行（预期 1），迁移将回滚"
        )));
    }
    // 提交前读回版本，事务内可见值必须已是 v2（如 AFTER 触发器改写等
    // 情况在此拦截）。
    let version_in_tx: String = tx.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    if version_in_tx != "2" {
        return Err(StoreError::Environment(format!(
            "迁移事务内版本读回异常：{version_in_tx}（预期 2），迁移将回滚"
        )));
    }
    tx.commit()?;
    Ok(())
}

/// 全新库的 schema 建立；两个连接并发首次打开时由 IMMEDIATE 写锁序列化，
/// `CREATE IF NOT EXISTS` / `INSERT OR IGNORE` 保证幂等。
fn ensure_schema(conn: &mut Connection) -> Result<(), StoreError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if has_meta {
        return verify_schema(conn);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA_SQL)?;
    tx.execute_batch(stage_run::STAGE_SCHEMA)?;
    tx.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION.to_string()],
    )?;
    tx.commit()?;
    verify_schema(conn)
}

/// Expected tables/columns derived from the same SQL used for a fresh instance.
fn expected_schema() -> Result<Vec<(String, Vec<String>)>, StoreError> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute_batch(stage_run::STAGE_SCHEMA)?;
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    drop(statement);
    let mut tables = Vec::new();
    for name in names {
        let mut statement = conn.prepare(&format!("PRAGMA table_info('{name}')"))?;
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(1))?
            .collect::<Result<_, _>>()?;
        tables.push((name, columns));
    }
    Ok(tables)
}

/// An existing instance must contain every table and column a fresh schema
/// creates. Missing pieces are an environment error, never silently repaired.
fn verify_schema(conn: &Connection) -> Result<(), StoreError> {
    for (table, columns) in expected_schema()? {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [&table],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(StoreError::Environment(format!(
                "实例数据库缺少必需表 {table}"
            )));
        }
        let mut statement = conn.prepare(&format!("PRAGMA table_info('{table}')"))?;
        let actual: Vec<String> = statement
            .query_map([], |row| row.get(1))?
            .collect::<Result<_, _>>()?;
        for column in columns {
            if !actual.iter().any(|name| *name == column) {
                return Err(StoreError::Environment(format!(
                    "实例数据库表 {table} 缺列 {column}"
                )));
            }
        }
    }
    Ok(())
}

/// Compare two persisted JSON string arrays as sets. Plan scope/rules are
/// order-insensitive semantic sets; malformed persisted JSON is a data error,
/// not a missing match, so callers fail closed with `Environment`.
fn same_string_set(left: &str, right: &str, field: &str) -> Result<bool, StoreError> {
    let parse = |text: &str| {
        serde_json::from_str::<Vec<String>>(text).map_err(|error| {
            StoreError::Environment(format!("{field} 不是字符串数组（数据库不一致）：{error}"))
        })
    };
    let (mut left, mut right) = (parse(left)?, parse(right)?);
    left.sort();
    right.sort();
    Ok(left == right)
}

/// 语义参数的规范化文本（WP1-A：submit 的语义参数只有 plan_id）。
fn canon_params(plan_id: &str) -> String {
    format!("{{\"plan_id\":\"{plan_id}\"}}")
}

/// 重试语义参数的规范化文本：expected_revision、plan_id、task_id
///（14：不同的 expected_revision 也属于参数不同）。
fn canon_retry_params(task_id: &str, expected_revision: u64, plan_id: &str) -> String {
    format!("{{\"expected_revision\":{expected_revision},\"plan_id\":\"{plan_id}\",\"task_id\":\"{task_id}\"}}")
}

/// FNV-1a 64 位摘要（非加密）：只作索引与墓碑标识，判定以 `params_canon`
/// 精确相等为准。
fn params_hash(canon: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canon.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 生成本地 task ID：优先 /dev/urandom，失败退回时间+计数混合（仅保证本地唯一性）。
fn new_task_id() -> String {
    let mut bytes = [0u8; 6];
    let mut random_ok = false;
    if let Ok(mut file) = File::open("/dev/urandom") {
        random_ok = file.read_exact(&mut bytes).is_ok();
    }
    if !random_ok {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = nanos.rotate_left(24) ^ SEQ.fetch_add(1, Ordering::Relaxed);
        bytes = mixed.to_le_bytes()[..6].try_into().expect("8 字节切片取 6");
    }
    let mut id = String::with_capacity(5 + 12);
    id.push_str("task_");
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// 状态文本 ↔ [`State`] 的落库映射；未知文本视为数据损坏。
fn parse_state(text: &str) -> Result<State, StoreError> {
    let state = match text {
        "queued" => State::Queued,
        "preparing" => State::Preparing,
        "launching" => State::Launching,
        "running" => State::Running,
        "verifying" => State::Verifying,
        "cancelling" => State::Cancelling,
        "awaiting_review" => State::AwaitingReview,
        "blocked" => State::Blocked,
        "failed" => State::Failed,
        "cancelled" => State::Cancelled,
        "interrupted" => State::Interrupted,
        "rejected" => State::Rejected,
        "completed" => State::Completed,
        _ => {
            return Err(StoreError::Environment(format!(
                "无法识别的状态文本：{text}"
            )))
        }
    };
    Ok(state)
}

/// opaque ID：非空、≤128 字节，仅 ASCII 字母、数字和 `-_.:`。
fn validate_id(value: &str, field: &'static str) -> Result<(), StoreError> {
    if value.is_empty() || value.len() > 128 {
        return Err(StoreError::InvalidInput(format!(
            "{field} 长度须在 1..=128 字节"
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(StoreError::InvalidInput(format!(
            "{field} 只允许 ASCII 字母、数字和 -_.:"
        )));
    }
    Ok(())
}

/// 04：OID 为完整小写十六进制 commit ID（40 字符）。
fn validate_oid(value: &str) -> Result<(), StoreError> {
    let lowercase_hex = value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if lowercase_hex {
        return Ok(());
    }
    Err(StoreError::InvalidInput(
        "base_oid 必须是 40 个小写十六进制字符的完整 commit OID".into(),
    ))
}

/// 时间字段校验：必须可解析为 UTC RFC3339（仅 `Z` 时区、秒值 00–59）。
fn require_time(value: &str, field: &'static str) -> Result<(), StoreError> {
    if parse_rfc3339_utc(value).is_some() {
        return Ok(());
    }
    Err(StoreError::InvalidInput(format!(
        "{field} 不是有效的 UTC RFC3339 时间：{value}"
    )))
}

/// 时间输入长度上限（字节）：19 字节日期时间 + '.' + 至多 19 位小数 + 'Z'。
const TIME_MAX_LEN: usize = 40;
/// 长度上限内小数秒的最大位数；小数秒缩放到该精度后可按数值直接比较。
const TIME_MAX_FRAC_DIGITS: u32 = 19;
const _: () = assert!(TIME_MAX_LEN - 21 == TIME_MAX_FRAC_DIGITS as usize);

/// 解析后的 UTC RFC3339 时间点：自 1970-01-01T00:00:00Z 的秒数，加上
/// 缩放到 [`TIME_MAX_FRAC_DIGITS`] 位定点的小数秒。派生顺序即时间先后；
/// 尾零缩放后自然相等（.1Z 与 .100Z 同刻度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimePoint {
    epoch_second: i64,
    frac_scaled: u128,
}

/// 解析 UTC RFC3339 时间为 [`TimePoint`]。仅接受 `Z`/`z` 时区（04：时间
/// UTC RFC3339，非 UTC 偏移拒绝）；分隔符接受 `T`/`t`；小数秒 1..=19 位
///（长度上限内）按精确值参与比较，不截断；秒值只接受 00–59：WP1-A 的
/// 计划时钟不支持闰秒 60，也不折算为下一分钟。日历无效（如 2 月 30 日、
/// 非闰年 2 月 29 日）、字段越界或超出长度上限返回 None。
fn parse_rfc3339_utc(value: &str) -> Option<TimePoint> {
    if !value.is_ascii() || value.len() > TIME_MAX_LEN {
        return None;
    }
    let b = value.as_bytes();
    let digits = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    if b.len() < 20 {
        return None;
    }
    let year = digits(&value[0..4])? as i64;
    let sep_ok = b[10] == b'T' || b[10] == b't';
    if b[4] != b'-' || b[7] != b'-' || !sep_ok || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let month = digits(&value[5..7])? as u32;
    let day = digits(&value[8..10])? as u32;
    let hour = digits(&value[11..13])? as u32;
    let minute = digits(&value[14..16])? as u32;
    let second = digits(&value[17..19])? as u32;
    let (frac_scaled, tail_start) = if b[19] == b'.' {
        let mut end = 20;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        if end == 20 || end >= b.len() {
            return None;
        }
        let ndigits = (end - 20) as u32;
        let mut frac: u128 = 0;
        for byte in &b[20..end] {
            frac = frac * 10 + u128::from(byte - b'0');
        }
        // ndigits ≤ TIME_MAX_FRAC_DIGITS（长度上限保证），
        // 乘积 < 10^19，远低于 u128 上限，不会溢出。
        (frac * 10u128.pow(TIME_MAX_FRAC_DIGITS - ndigits), end)
    } else {
        (0, 19)
    };
    if b[tail_start] != b'Z' && b[tail_start] != b'z' {
        return None;
    }
    if tail_start + 1 != b.len() {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let epoch = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    Some(TimePoint {
        epoch_second: epoch,
        frac_scaled,
    })
}

/// 纯数字十进制串 → u64；空串、非 ASCII 数字或带符号（如 `+1`）一律 None。
fn parse_digits(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// 公历闰年：四年一闰，百年不闰，四百年再闰。
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// 自 1970-01-01 起的天数（Hinnant days_from_civil）。
/// 146097 天恰为 400 个格里高利年，故 era 以 400 年为单位。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 从首次提交保存的 result_json 快照中取出 (状态文本, revision,
/// active_attempt)。仅接受本 crate 写入的规范化形态（04 成功结果最小
/// 样例）；其余视为数据损坏。重放的结构化字段由此与 result_json 同源。
fn parse_result_snapshot(json: &str) -> Result<(&str, u64, u32), StoreError> {
    let rest = json
        .strip_prefix("{\"schema_version\":1,\"task\":{\"id\":\"")
        .ok_or_else(|| snapshot_error(json))?;
    let id_end = rest.find('"').ok_or_else(|| snapshot_error(json))?;
    let _task_id = &rest[..id_end];
    let rest = rest[id_end..]
        .strip_prefix("\",\"state\":\"")
        .ok_or_else(|| snapshot_error(json))?;
    let state_end = rest.find('"').ok_or_else(|| snapshot_error(json))?;
    let state_text = &rest[..state_end];
    let rest = rest[state_end..]
        .strip_prefix("\",\"revision\":")
        .ok_or_else(|| snapshot_error(json))?;
    let rev_end = rest.find(',').ok_or_else(|| snapshot_error(json))?;
    let revision = parse_digits(&rest[..rev_end]).ok_or_else(|| snapshot_error(json))?;
    let rest = rest[rev_end..]
        .strip_prefix(",\"active_attempt\":")
        .ok_or_else(|| snapshot_error(json))?;
    let attempt_end = rest.find('}').ok_or_else(|| snapshot_error(json))?;
    let active_attempt =
        parse_digits(&rest[..attempt_end]).ok_or_else(|| snapshot_error(json))? as u32;
    if revision == 0 || active_attempt == 0 {
        return Err(snapshot_error(json));
    }
    if &rest[attempt_end..] != "}}" {
        return Err(snapshot_error(json));
    }
    Ok((state_text, revision, active_attempt))
}

/// result_json 快照损坏（非本 crate 写入的规范化形态）。
fn snapshot_error(json: &str) -> StoreError {
    StoreError::Environment(format!("结果快照无法解析：{json}"))
}

/// 04：goal ≤ 1 KiB，brief ≤ 16 KiB（字节长度，编码后计）。
fn validate_text(value: &str, max_bytes: usize, field: &'static str) -> Result<(), StoreError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(StoreError::InvalidInput(format!(
            "{field} 长度须在 1..={max_bytes} 字节"
        )));
    }
    Ok(())
}

/// bundled libsqlite3 的版本字符串（03 要求记录实际版本）。
pub fn sqlite_version() -> &'static str {
    rusqlite::version()
}

#[cfg(test)]
mod tests {
    use relay_core::State;

    use super::{
        event_kind, params_hash, parse_result_snapshot, parse_rfc3339_utc, parse_state, TimePoint,
    };
    use relay_core::Event;

    #[test]
    fn event_kind_is_snake_case_of_variant_name() {
        assert_eq!(event_kind(Event::Claim), "claim");
        assert_eq!(event_kind(Event::CancelRequested), "cancel_requested");
        assert_eq!(
            event_kind(Event::AllOwnedWorkersStopped),
            "all_owned_workers_stopped"
        );
        assert_eq!(event_kind(Event::PreflightBlocked), "preflight_blocked");
        for event in Event::ALL {
            let kind = event_kind(*event);
            assert!(!kind.is_empty());
            assert!(
                kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{kind} 应为小写蛇形"
            );
            assert!(!kind.starts_with('_') && !kind.ends_with('_'));
        }
    }

    #[test]
    fn state_text_round_trips_for_every_state() {
        for state in State::ALL {
            let text = state.to_string();
            assert_eq!(parse_state(&text).unwrap(), *state);
        }
        assert!(parse_state("bogus").is_err());
    }

    #[test]
    fn params_hash_is_stable_and_covers_input() {
        let same = "{\"plan_id\":\"p1\"}";
        assert_eq!(params_hash(same), params_hash(same));
        assert_ne!(
            params_hash("{\"plan_id\":\"p1\"}"),
            params_hash("{\"plan_id\":\"p2\"}")
        );
    }

    #[test]
    fn rfc3339_orders_by_instant_not_string() {
        let p = parse_rfc3339_utc;
        // 纪元锚点与秒差
        assert_eq!(
            p("1970-01-01T00:00:00Z"),
            Some(TimePoint {
                epoch_second: 0,
                frac_scaled: 0
            })
        );
        assert_eq!(
            p("1970-01-02T00:00:00Z"),
            Some(TimePoint {
                epoch_second: 86_400,
                frac_scaled: 0
            })
        );
        let whole = p("2026-11-22T01:00:00Z").unwrap();
        assert_eq!(
            p("2026-11-22T01:00:01Z").unwrap().epoch_second,
            whole.epoch_second + 1
        );
        // 原缺陷回归：整秒 now 早于小数秒 expires_at，字典序会误判先后
        let frac = p("2026-11-22T01:00:00.500Z").unwrap();
        assert!(whole < frac);
        // 同一刻度不同精度写法相等；尾零不改变时间点（.1Z == .100Z）
        assert_eq!(p("2026-11-22T01:00:00Z"), p("2026-11-22T01:00:00.000Z"));
        assert_eq!(p("2026-11-22T01:00:00.500Z"), p("2026-11-22T01:00:00.5Z"));
        assert_eq!(p("2026-11-22T01:00:00.1Z"), p("2026-11-22T01:00:00.100Z"));
        // 超过 9 位的小数不截断：第 10 位参与比较
        let fine = p("2026-11-22T01:00:00.1234567890Z").unwrap();
        let finer = p("2026-11-22T01:00:00.1234567891Z").unwrap();
        assert!(fine < finer);
        // 小写 z/t 接受；闰年 2 月 29 日接受
        assert!(p("2026-11-22t01:00:00z").is_some());
        assert!(p("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn rfc3339_rejects_invalid_input() {
        for bad in [
            "",
            "not-a-time",
            "2026-02-30T00:00:00Z",                       // 不存在的日期
            "2026-02-29T00:00:00Z",                       // 2026 非闰年
            "2026-13-01T00:00:00Z",                       // 月份越界
            "2026-00-10T00:00:00Z",                       // 月份为 0
            "2026-11-00T00:00:00Z",                       // 日为 0
            "2026-11-22T24:00:00Z",                       // 小时越界
            "2026-11-22T01:60:00Z",                       // 分钟越界
            "2026-11-22T01:00:60Z",                       // 秒值 60（WP1-A 不支持闰秒）
            "2026-11-22T01:00:61Z",                       // 秒越界
            "2016-12-31T23:59:60Z",                       // 真实闰秒时刻同样拒绝
            "2026-11-22T01:00:00+08:00",                  // 非 UTC 偏移
            "2026-11-22 01:00:00Z",                       // 空格分隔符
            "2026-11-22T01:00:00",                        // 缺时区
            "2026-11-22T01:00:00.5",                      // 缺时区
            "2026-11-22T01:00:00.Z",                      // 小数点后无数字
            "2026-11-22T01:00:00ZZ",                      // 时区后多余内容
            "2026-11-22T01:00:00.123456789012345678901Z", // 超出长度上限
        ] {
            assert!(parse_rfc3339_utc(bad).is_none(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn result_snapshot_parses_canonical_shape() {
        assert_eq!(
            parse_result_snapshot(
                "{\"schema_version\":1,\"task\":{\"id\":\"task_ab12\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}}"
            )
            .unwrap(),
            ("queued", 1, 1)
        );
        assert_eq!(
            parse_result_snapshot(
                "{\"schema_version\":1,\"task\":{\"id\":\"task_x\",\"state\":\"awaiting_review\",\"revision\":6,\"active_attempt\":2}}"
            )
            .unwrap(),
            ("awaiting_review", 6, 2)
        );
        for bad in [
            "{\"schema_version\":1,\"task\":{\"id\":\"t\",\"state\":\"queued\",\"revision\":0,\"active_attempt\":1}}",
            "{\"schema_version\":1,\"task\":{\"id\":\"t\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":0}}",
            "{\"schema_version\":1,\"task\":{\"id\":\"t\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1},\"extra\":1}",
            "{\"schema_version\":2,\"task\":{\"id\":\"t\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}}",
            "{\"schema_version\":1,\"task\":{\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}}",
            "{\"schema_version\":1,\"task\":{\"id\":\"t\",\"state\":\"queued\",\"revision\":1,\"active_attempt\":1}",
            "",
        ] {
            assert!(parse_result_snapshot(bad).is_err(), "{bad} 应被拒绝");
        }
    }
}

// ---- M1 制品登记、事件查询、任务列表与验收（schema v3） ----

/// 已登记计划记录（04 relay_plan → relay_submit 的冻结快照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRecord {
    pub plan_id: String,
    pub project_id: String,
    pub goal: String,
    pub brief: String,
    pub base_oid: String,
    pub created_at: String,
    pub expires_at: String,
    /// "unused" / "used"。
    pub status: String,
    pub write_scope: Vec<String>,
    pub rule_ids: Vec<String>,
    pub profile_id: String,
}

/// 制品登记输入（03/17：制品引用与状态推进同一 IMMEDIATE 事务）。
#[derive(Debug, Clone, Copy)]
pub struct ArtifactRegisterInput<'a> {
    pub task_id: &'a str,
    pub attempt_no: u32,
    pub expected_revision: u64,
    /// 已持久发布的制品目录（绝对或受管根内路径，存引用不复制内容）。
    pub artifact_dir: &'a str,
    pub manifest_sha256: &'a str,
    pub patch_sha256: &'a str,
    pub receipt_sha256: &'a str,
    pub handoff_sha256: &'a str,
    pub now: &'a str,
}

/// 已登记制品引用（查询契约的唯一事实来源；未登记目录不可被发现）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub task_id: String,
    pub attempt_no: u32,
    pub artifact_dir: String,
    pub manifest_sha256: String,
    pub patch_sha256: String,
    pub receipt_sha256: String,
    pub handoff_sha256: String,
    pub created_at: String,
}

/// 事件查询行（04：events.list 短轮询；与 revision 一起去重）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub seq: u64,
    pub task_id: String,
    pub attempt_no: u32,
    pub revision: u64,
    pub kind: String,
    pub state: String,
    pub reason_code: Option<String>,
    pub created_at: String,
}

/// 验收输入（04 relay_review）：必须 awaiting_review，核对本机摘要；
/// request_id 幂等——重放返回首次结果，同 ID 异参拒绝。
#[derive(Debug, Clone, Copy)]
pub struct ReviewInput<'a> {
    pub task_id: &'a str,
    pub attempt_no: u32,
    pub expected_revision: u64,
    pub request_id: &'a str,
    /// "accept" / "reject"。
    pub verdict: &'a str,
    pub diff_sha256: &'a str,
    pub test_sha256: &'a str,
    pub note: &'a str,
    pub now: &'a str,
}

/// 取消输入（04 relay_cancel）：request_id 幂等语义同上。
#[derive(Debug, Clone, Copy)]
pub struct CancelInput<'a> {
    pub task_id: &'a str,
    pub expected_revision: u64,
    pub request_id: &'a str,
    pub now: &'a str,
}

/// 项目任务列表分页（04 relay_list）：稳定游标绑定查询范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<TaskSummary>,
    /// 下一页游标（本页最后一个 task_id）；无更多任务时为 None。
    pub next_cursor: Option<String>,
}

fn validate_sha256(value: &str, field: &str) -> Result<(), StoreError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(StoreError::InvalidInput(format!(
            "{field} 必须是 64 个小写十六进制字符"
        )))
    }
}

impl Store {
    /// 制品登记 + 状态推进同一事务（03/17）：在一个 IMMEDIATE 事务内
    /// 完成 CAS 状态推进（当前 attempt 必须 verifying，经
    /// [`Event::VerificationPassed`] 到 awaiting_review）与制品行插入；
    /// 任一步失败整体回滚——调用方（service）据回滚移除已落盘目录，
    /// 未登记制品不可见。同一 attempt 至多登记一次（主键冲突报错）。
    pub fn register_artifact_and_transition(
        &self,
        input: ArtifactRegisterInput<'_>,
    ) -> Result<TransitionOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        if input.attempt_no == 0 || input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "attempt_no 与 expected_revision 必须为正整数".into(),
            ));
        }
        if input.artifact_dir.is_empty() {
            return Err(StoreError::InvalidInput("artifact_dir 不能为空".into()));
        }
        validate_sha256(input.manifest_sha256, "manifest_sha256")?;
        validate_sha256(input.patch_sha256, "patch_sha256")?;
        validate_sha256(input.receipt_sha256, "receipt_sha256")?;
        validate_sha256(input.handoff_sha256, "handoff_sha256")?;
        require_time(input.now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match register_artifact_in_tx(&tx, input) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 查询已登记计划（提交时冻结的执行快照：brief/scope/rules/profile）。
    pub fn get_plan(&self, plan_id: &str) -> Result<Option<PlanRecord>, StoreError> {
        validate_id(plan_id, "plan_id")?;
        self.lock_conn()?
            .query_row(
                "SELECT plan_id, project_id, goal, brief, base_oid, created_at, expires_at, status,
                        write_scope, rule_ids, profile_id
                 FROM plans WHERE plan_id = ?1",
                params![plan_id],
                |r| {
                    let scope_text: String = r.get(8)?;
                    let rules_text: String = r.get(9)?;
                    let parse_ids = |text: &str| -> rusqlite::Result<Vec<String>> {
                        serde_json::from_str(text).map_err(|_| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                "plan 快照 JSON 损坏".into(),
                            )
                        })
                    };
                    Ok(PlanRecord {
                        plan_id: r.get(0)?,
                        project_id: r.get(1)?,
                        goal: r.get(2)?,
                        brief: r.get(3)?,
                        base_oid: r.get(4)?,
                        created_at: r.get(5)?,
                        expires_at: r.get(6)?,
                        status: r.get(7)?,
                        write_scope: parse_ids(&scope_text)?,
                        rule_ids: parse_ids(&rules_text)?,
                        profile_id: r.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 查询任务对应的计划 ID（冻结快照的引用；tasks.plan_id 列）。
    pub fn get_task_plan_id(&self, task_id: &str) -> Result<Option<String>, StoreError> {
        validate_id(task_id, "task_id")?;
        self.lock_conn()?
            .query_row(
                "SELECT plan_id FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// 显式取消（04 relay_cancel）：request_id 去重最先——同 ID 同参重放
    /// 返回首次成功取消的结果快照（不新增事件），同 ID 异参报
    /// IDEMPOTENCY_CONFLICT。非重放路径：CAS 推进
    /// CancelRequested（queued/preparing/awaiting_review → cancelled；
    /// launching/running/verifying → cancelling），在同一 IMMEDIATE
    /// 事务内写入成功 request 快照。失败整体回滚、不留请求记录。
    pub fn cancel(&self, input: CancelInput<'_>) -> Result<SubmitOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        if input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "expected_revision 必须为正整数".into(),
            ));
        }
        validate_id(input.request_id, "request_id")?;
        require_time(input.now, "now")?;
        let canon = format!(
            "{{\"task_id\":{},\"expected_revision\":{}}}",
            serde_json::to_string(input.task_id).unwrap_or_default(),
            input.expected_revision
        );
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match cancel_in_tx(&tx, input, &canon) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 查询已登记制品；未登记返回 None（查询不扫描目录）。
    pub fn get_artifact(
        &self,
        task_id: &str,
        attempt_no: u32,
    ) -> Result<Option<ArtifactRecord>, StoreError> {
        validate_id(task_id, "task_id")?;
        self.lock_conn()?
            .query_row(
                "SELECT task_id, attempt_no, artifact_dir, manifest_sha256, patch_sha256,
                        receipt_sha256, handoff_sha256, created_at
                 FROM artifacts WHERE task_id = ?1 AND attempt_no = ?2",
                params![task_id, i64::from(attempt_no)],
                artifact_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Number of tasks that still occupy the local serial queue. Terminal
    /// completed/rejected/cancelled/failed tasks are excluded; blocked,
    /// interrupted and awaiting-review tasks still count because they need a
    /// human decision and must not silently grow without bound.
    pub fn count_nonterminal_tasks(&self) -> Result<u64, StoreError> {
        let count: i64 = self.lock_conn()?.query_row(
            "SELECT COUNT(*) FROM tasks WHERE state NOT IN
                ('completed','rejected','cancelled','failed')",
            [],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// 最近更新的任务（跨项目，供本机任务窗口总览）：按 updated_at
    /// 倒序、task_id 倒序稳定排序；limit 1..=100。
    pub fn list_recent_tasks(&self, limit: u32) -> Result<Vec<TaskSummary>, StoreError> {
        if limit == 0 || limit > 100 {
            return Err(StoreError::InvalidInput("limit 须在 1..=100".into()));
        }
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, project_id, goal, state, revision, active_attempt,
                    reason_code, created_at, updated_at
             FROM tasks ORDER BY updated_at DESC, task_id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![i64::from(limit)], task_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// 追加一条过程事件（自动链路的轮次记录等）：绑定任务当前状态与
    /// revision，**不改状态、不推进 revision**。事件 seq 仍单调递增，
    /// events.list 按 seq 游标可见。
    pub fn append_event(
        &self,
        task_id: &str,
        attempt_no: u32,
        kind: &str,
        reason: Option<&str>,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        if attempt_no == 0 {
            return Err(StoreError::InvalidInput("attempt_no 必须为正".into()));
        }
        if kind.trim().is_empty() || kind.len() > 64 {
            return Err(StoreError::InvalidInput(
                "kind 须为 1..=64 字节的非空文本".into(),
            ));
        }
        require_time(now, "now")?;
        let conn = self.lock_conn()?;
        let (state_text, revision): (String, i64) = conn
            .query_row(
                "SELECT state, revision FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                task_id: task_id.to_owned(),
            })?;
        conn.execute(
            "INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_id,
                i64::from(attempt_no),
                revision,
                kind,
                state_text,
                reason,
                now
            ],
        )?;
        Ok(())
    }

    /// 事件短轮询（04 events.list）：返回 seq 严格大于 after_seq 的事件，
    /// 按 seq 升序；limit 1..=100。
    pub fn list_events(&self, after_seq: u64, limit: u32) -> Result<Vec<EventRecord>, StoreError> {
        if limit == 0 || limit > 100 {
            return Err(StoreError::InvalidInput("limit 须在 1..=100".into()));
        }
        let after = i64::try_from(after_seq)
            .map_err(|_| StoreError::InvalidInput("after_seq 超出可表示范围".into()))?;
        let stmt_rows: Vec<EventRecord> = self
            .lock_conn()?
            .prepare(
                "SELECT seq, task_id, attempt_no, revision, kind, state, reason_code, created_at
                 FROM events WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
            )?
            .query_map(params![after, i64::from(limit)], event_row)?
            .collect::<Result<_, _>>()?;
        Ok(stmt_rows)
    }

    /// 项目任务列表（04 relay_list）：稳定分页游标绑定查询范围——游标
    /// 为上一页最后一个 task_id，同序同范围重查结果一致；limit 1..=50。
    pub fn list_tasks(
        &self,
        project_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<TaskPage, StoreError> {
        validate_id(project_id, "project_id")?;
        if let Some(c) = cursor {
            validate_id(c, "cursor")?;
        }
        if limit == 0 || limit > 50 {
            return Err(StoreError::InvalidInput("limit 须在 1..=50".into()));
        }
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, project_id, goal, state, revision, active_attempt,
                    reason_code, created_at, updated_at
             FROM tasks WHERE project_id = ?1 AND (?2 IS NULL OR task_id > ?2)
             ORDER BY task_id ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![project_id, cursor, i64::from(limit)], task_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if rows.len() == limit as usize {
            rows.last().map(|t: &TaskSummary| t.task_id.clone())
        } else {
            None
        };
        Ok(TaskPage {
            tasks: rows,
            next_cursor,
        })
    }

    /// 显式验收（04 relay_review）：request_id 去重最先——同 ID 同参
    /// 重放返回首次成功验收的结果快照（不新增事件），同 ID 异参报
    /// IDEMPOTENCY_CONFLICT。非重放路径在一个 IMMEDIATE 事务内核对
    /// awaiting_review 状态、CAS 预期 revision、attempt 已登记制品且
    /// 本机摘要与登记一致，再按 verdict 推进 ReviewAccepted/ReviewRejected。
    /// 摘要不符或制品未登记拒绝且不推进（04：不放行，不转成通过）。
    pub fn review(&self, input: ReviewInput<'_>) -> Result<SubmitOutcome, StoreError> {
        validate_id(input.task_id, "task_id")?;
        if input.attempt_no == 0 || input.expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "attempt_no 与 expected_revision 必须为正整数".into(),
            ));
        }
        if input.verdict != "accept" && input.verdict != "reject" {
            return Err(StoreError::InvalidInput(
                "verdict 只允许 accept 或 reject".into(),
            ));
        }
        validate_id(input.request_id, "request_id")?;
        validate_sha256(input.diff_sha256, "diff_sha256")?;
        validate_sha256(input.test_sha256, "test_sha256")?;
        if input.note.len() > 2 * 1024 {
            return Err(StoreError::InvalidInput("note 超 2 KiB".into()));
        }
        require_time(input.now, "now")?;
        let canon = format!(
            "{{\"task_id\":{},\"attempt_no\":{},\"expected_revision\":{},\"verdict\":{},\"diff_sha256\":{},\"test_sha256\":{}}}",
            serde_json::to_string(input.task_id).unwrap_or_default(),
            input.attempt_no,
            input.expected_revision,
            serde_json::to_string(input.verdict).unwrap_or_default(),
            serde_json::to_string(input.diff_sha256).unwrap_or_default(),
            serde_json::to_string(input.test_sha256).unwrap_or_default()
        );
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match review_in_tx(&tx, input, &canon) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }
}

/// v4 → v5 迁移：建 managed_groups 表（持久派发门）；备份与既有迁移同法。
pub fn migrate_v4_to_v5(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(4) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(5) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v5，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v4，请先迁移到 v4"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v4 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS gate_resolutions (
    task_id    TEXT NOT NULL REFERENCES tasks(task_id),
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    evidence   TEXT NOT NULL,
    result     TEXT NOT NULL,
    resolved_by TEXT NOT NULL,
    resolved_at TEXT NOT NULL,
    PRIMARY KEY (task_id, attempt_no)
);

CREATE TABLE IF NOT EXISTS managed_groups (
            task_id      TEXT NOT NULL REFERENCES tasks(task_id),
            attempt_no  INTEGER NOT NULL CHECK (attempt_no > 0),
            pgid        INTEGER NOT NULL,
            leader_pid  INTEGER NOT NULL,
            leader_start TEXT NOT NULL,
            state       TEXT NOT NULL CHECK (state IN ('tracking','blocking','resolved')),
            reason      TEXT,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (task_id, attempt_no)
        );",
    )?;
    tx.execute(
        "UPDATE meta SET value = '5' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// v7 → v8 迁移：dispatch_gate 增 pgid 列（probe 等未登记组的进程号
/// 归属证据，供重启后探测确认退出）；备份与既有迁移同法。
pub fn migrate_v7_to_v8(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(7) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(8) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v8，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v7，请先迁移到 v7"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v7 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("ALTER TABLE dispatch_gate ADD COLUMN pgid INTEGER NOT NULL DEFAULT 0;")?;
    tx.execute(
        "UPDATE meta SET value = '8' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// v6 → v7 迁移：建 gate_resolutions（受管恢复审计表）；备份与既有迁移同法。
pub fn migrate_v6_to_v7(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(6) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(7) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v7，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v6，请先迁移到 v6"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v6 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS gate_resolutions (
            task_id    TEXT NOT NULL REFERENCES tasks(task_id),
            attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
            evidence   TEXT NOT NULL,
            result     TEXT NOT NULL,
            resolved_by TEXT NOT NULL,
            resolved_at TEXT NOT NULL,
            PRIMARY KEY (task_id, attempt_no)
        );",
    )?;
    tx.execute(
        "UPDATE meta SET value = '7' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// v5 → v6 迁移：managed_groups 增 'pending' 状态（fence 同事务预登记，
/// spawn 前持久化）；SQLite 无法改 CHECK，重建表。备份与既有迁移同法。
pub fn migrate_v5_to_v6(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    match read_schema_version(&conn)? {
        Some(5) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(6) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v6，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v5，请先迁移到 v5"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v5 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS dispatch_gate (
            task_id    TEXT NOT NULL REFERENCES tasks(task_id),
            attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
            state      TEXT NOT NULL CHECK (state IN ('pending','released')),
            reason     TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (task_id, attempt_no)
        );
        CREATE TABLE managed_groups_v6 (
            task_id      TEXT NOT NULL REFERENCES tasks(task_id),
            attempt_no   INTEGER NOT NULL CHECK (attempt_no > 0),
            pgid         INTEGER NOT NULL,
            leader_pid   INTEGER NOT NULL,
            leader_start TEXT NOT NULL,
            state        TEXT NOT NULL CHECK (state IN ('tracking','blocking','resolved')),
            reason       TEXT,
            created_at   TEXT NOT NULL,
            updated_at   TEXT NOT NULL,
            PRIMARY KEY (task_id, attempt_no)
        );
        INSERT INTO managed_groups_v6
            SELECT task_id, attempt_no, pgid, leader_pid, leader_start,
                   CASE WHEN state = 'pending' THEN 'tracking' ELSE state END, reason,
                   created_at, updated_at FROM managed_groups;
        DROP TABLE managed_groups;
        ALTER TABLE managed_groups_v6 RENAME TO managed_groups;",
    )?;
    tx.execute(
        "UPDATE meta SET value = '6' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// v3 → v4 迁移：artifacts 增 handoff_sha256 列；备份与既有迁移同法。
pub fn migrate_v3_to_v4(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(3) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(4) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v4，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v3，请先迁移到 v3"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v3 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("ALTER TABLE artifacts ADD COLUMN handoff_sha256 TEXT NOT NULL DEFAULT '';")?;
    tx.execute(
        "UPDATE meta SET value = '4' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn artifact_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRecord> {
    Ok(ArtifactRecord {
        task_id: r.get(0)?,
        attempt_no: r.get::<_, i64>(1)? as u32,
        artifact_dir: r.get(2)?,
        manifest_sha256: r.get(3)?,
        patch_sha256: r.get(4)?,
        receipt_sha256: r.get(5)?,
        handoff_sha256: r.get(6)?,
        created_at: r.get(7)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    Ok(EventRecord {
        seq: r.get::<_, i64>(0)? as u64,
        task_id: r.get(1)?,
        attempt_no: r.get::<_, i64>(2)? as u32,
        revision: r.get::<_, i64>(3)? as u64,
        kind: r.get(4)?,
        state: r.get(5)?,
        reason_code: r.get(6)?,
        created_at: r.get(7)?,
    })
}

fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSummary> {
    let state_text: String = r.get(3)?;
    let state = parse_state(&state_text).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("未知任务状态 {state_text}").into(),
        )
    })?;
    Ok(TaskSummary {
        task_id: r.get(0)?,
        project_id: r.get(1)?,
        goal: r.get(2)?,
        state,
        revision: r.get::<_, i64>(4)? as u64,
        active_attempt: r.get::<_, i64>(5)? as u32,
        reason_code: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

fn register_artifact_in_tx(
    tx: &Transaction<'_>,
    input: ArtifactRegisterInput<'_>,
) -> Result<TransitionOutcome, StoreError> {
    stage_run::require_reviewed_candidate(
        tx,
        input.task_id,
        input.attempt_no,
        input.patch_sha256,
        input.receipt_sha256,
    )?;
    // 1. CAS 状态推进：verifying → awaiting_review（事件 VerificationPassed）。
    let outcome = transition_in_tx(
        tx,
        TransitionInput {
            task_id: input.task_id,
            expected_active_attempt: input.attempt_no,
            expected_revision: input.expected_revision,
            event: Event::VerificationPassed,
            now: input.now,
        },
    )?;
    // 2. 制品行插入；同 attempt 重复登记（主键冲突）整体回滚。
    tx.execute(
        "INSERT INTO artifacts (task_id, attempt_no, artifact_dir, manifest_sha256,
                                patch_sha256, receipt_sha256, handoff_sha256, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.task_id,
            i64::from(input.attempt_no),
            input.artifact_dir,
            input.manifest_sha256,
            input.patch_sha256,
            input.receipt_sha256,
            input.handoff_sha256,
            input.now
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(ce, _)
            if ce.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Environment(format!(
                "attempt {}#{} 已登记制品，不得重复登记",
                input.task_id, input.attempt_no
            ))
        }
        other => other.into(),
    })?;
    Ok(outcome)
}

fn cancel_in_tx(
    tx: &Transaction<'_>,
    input: CancelInput<'_>,
    canon: &str,
) -> Result<SubmitOutcome, StoreError> {
    // 1. 去重最先：同 ID 同参重放首次结果；异参冲突。
    if let Some(replayed) = replay_request(tx, input.request_id, OP_CANCEL, canon)? {
        return Ok(replayed);
    }
    // 2. CAS 推进 CancelRequested。
    let outcome = transition_in_tx(
        tx,
        TransitionInput {
            task_id: input.task_id,
            expected_active_attempt: task_active_attempt(tx, input.task_id)?,
            expected_revision: input.expected_revision,
            event: Event::CancelRequested,
            now: input.now,
        },
    )?;
    // 3. 成功 request 快照（重放来源；不重复写事件）。
    let plan_id: String = tx.query_row(
        "SELECT plan_id FROM tasks WHERE task_id = ?1",
        params![input.task_id],
        |r| r.get(0),
    )?;
    let result_json = format!(
        "{{\"schema_version\":1,\"task\":{{\"id\":{},\"state\":{},\"revision\":{},\"active_attempt\":{}}}}}",
        serde_json::to_string(&outcome.task_id).unwrap_or_default(),
        serde_json::to_string(&outcome.state.to_string()).unwrap_or_default(),
        outcome.revision,
        outcome.active_attempt
    );
    tx.execute(
        "INSERT INTO requests (request_id, operation, params_canon, params_hash, plan_id,
                               task_id, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.request_id,
            OP_CANCEL,
            canon,
            params_hash(canon),
            plan_id,
            input.task_id,
            result_json,
            input.now
        ],
    )?;
    Ok(SubmitOutcome {
        task_id: outcome.task_id,
        state: outcome.state,
        revision: outcome.revision,
        active_attempt: outcome.active_attempt,
        created_at: input.now.to_owned(),
        deduplicated: false,
        result_json,
    })
}

/// 任务的当前 active_attempt（供取消 CAS；与 revision 一致性由
/// transition_in_tx 复核）。
fn task_active_attempt(tx: &Transaction<'_>, task_id: &str) -> Result<u32, StoreError> {
    let attempt: i64 = tx
        .query_row(
            "SELECT active_attempt FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            task_id: task_id.to_owned(),
        })?;
    Ok(attempt as u32)
}

fn review_in_tx(
    tx: &Transaction<'_>,
    input: ReviewInput<'_>,
    canon: &str,
) -> Result<SubmitOutcome, StoreError> {
    // 0. 去重最先：同 ID 同参重放首次结果；异参冲突。
    if let Some(replayed) = replay_request(tx, input.request_id, OP_REVIEW, canon)? {
        return Ok(replayed);
    }
    // 1. attempt 必须已登记制品（未登记目录不可被发现、不可被验收）。
    let registered: Option<(String, String)> = tx
        .query_row(
            "SELECT patch_sha256, receipt_sha256 FROM artifacts
             WHERE task_id = ?1 AND attempt_no = ?2",
            params![input.task_id, i64::from(input.attempt_no)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (patch_sha, receipt_sha) = registered.ok_or_else(|| {
        StoreError::InvalidInput(format!(
            "attempt {}#{} 无已登记制品，不能验收",
            input.task_id, input.attempt_no
        ))
    })?;
    // 2. 本机摘要与请求摘要一致；不一致不放行、不转成通过。
    if patch_sha != input.diff_sha256 {
        return Err(StoreError::InvalidInput(format!(
            "diff_sha256 与登记制品不符：登记 {patch_sha}"
        )));
    }
    if receipt_sha != input.test_sha256 {
        return Err(StoreError::InvalidInput(format!(
            "test_sha256 与登记制品不符：登记 {receipt_sha}"
        )));
    }
    // 3. CAS 推进：awaiting_review → completed / rejected。
    let event = if input.verdict == "accept" {
        Event::ReviewAccepted
    } else {
        Event::ReviewRejected
    };
    let outcome = transition_in_tx(
        tx,
        TransitionInput {
            task_id: input.task_id,
            expected_active_attempt: input.attempt_no,
            expected_revision: input.expected_revision,
            event,
            now: input.now,
        },
    )?;
    // 4. 人工验收备注：独立审计事件，不覆盖状态转换事件；空备注不写。
    if !input.note.is_empty() {
        tx.execute(
            "INSERT INTO events (task_id, attempt_no, revision, kind, state, reason_code, created_at)
             VALUES (?1, ?2, ?3, 'review_note', ?4, ?5, ?6)",
            params![
                input.task_id,
                i64::from(input.attempt_no),
                outcome.revision as i64,
                outcome.state.to_string(),
                input.note,
                input.now
            ],
        )?;
    }
    // 5. 成功 request 快照（重放来源；不重复写状态事件）。
    let plan_id: String = tx.query_row(
        "SELECT plan_id FROM tasks WHERE task_id = ?1",
        params![input.task_id],
        |r| r.get(0),
    )?;
    let result_json = format!(
        "{{\"schema_version\":1,\"task\":{{\"id\":{},\"state\":{},\"revision\":{},\"active_attempt\":{}}}}}",
        serde_json::to_string(&outcome.task_id).unwrap_or_default(),
        serde_json::to_string(&outcome.state.to_string()).unwrap_or_default(),
        outcome.revision,
        outcome.active_attempt
    );
    tx.execute(
        "INSERT INTO requests (request_id, operation, params_canon, params_hash, plan_id,
                               task_id, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            input.request_id,
            OP_REVIEW,
            canon,
            params_hash(canon),
            plan_id,
            input.task_id,
            result_json,
            input.now
        ],
    )?;
    Ok(SubmitOutcome {
        task_id: outcome.task_id,
        state: outcome.state,
        revision: outcome.revision,
        active_attempt: outcome.active_attempt,
        created_at: input.now.to_owned(),
        deduplicated: false,
        result_json,
    })
}

/// v2 → v3 迁移：建 artifacts 表并升版本；备份与 v1→v2 同法。
pub fn migrate_v2_to_v3(db_path: &Path, backup_path: &Path) -> Result<(), StoreError> {
    if backup_path.exists() {
        return Err(StoreError::InvalidInput(format!(
            "备份 {} 已存在，不覆盖已有备份",
            backup_path.display()
        )));
    }
    let mut conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    match read_schema_version(&conn)? {
        Some(2) => {}
        Some(found) if found > SCHEMA_VERSION => {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            })
        }
        Some(3) => {
            return Err(StoreError::InvalidInput(
                "数据库已是 schema v3，无需迁移".into(),
            ))
        }
        Some(found) => {
            return Err(StoreError::InvalidInput(format!(
                "数据库 schema v{found} 不是 v2，请先迁移到 v2"
            )))
        }
        None => {
            return Err(StoreError::InvalidInput(
                "meta 中缺少 schema_version，不是 v2 数据库".into(),
            ))
        }
    }
    let backup = backup_path.display().to_string();
    conn.execute("VACUUM INTO ?1", params![backup])?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS artifacts (
            task_id         TEXT NOT NULL REFERENCES tasks(task_id),
            attempt_no      INTEGER NOT NULL CHECK (attempt_no > 0),
            artifact_dir    TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL,
            patch_sha256    TEXT NOT NULL,
            receipt_sha256  TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            PRIMARY KEY (task_id, attempt_no)
        );
        ALTER TABLE plans ADD COLUMN write_scope TEXT NOT NULL DEFAULT '[]';
        ALTER TABLE plans ADD COLUMN rule_ids TEXT NOT NULL DEFAULT '[]';
        ALTER TABLE plans ADD COLUMN profile_id TEXT NOT NULL DEFAULT '';",
    )?;
    tx.execute(
        "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

// ---- M1 持久派发门：受管进程组持久登记（schema v5） ----

/// 受管恢复操作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateResolution {
    pub task_id: String,
    pub attempt_no: u32,
    pub evidence: String,
    /// "confirmed-exit"（可信退出证据）/ "operator-confirmed"（人工
    /// 确认进程不存在）/ "refused"（证据不足，保持阻断）。
    pub result: String,
    pub resolved_by: String,
    pub resolved_at: String,
}

/// 实例级待确认标记记录（dispatch_gate）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRecord {
    pub task_id: String,
    pub attempt_no: u32,
    pub state: String,
    /// 待确认期间相关进程组的 pgid（0 = 未知/未登记）。
    pub pgid: i32,
    pub reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 受管进程组记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedGroupRecord {
    pub task_id: String,
    pub attempt_no: u32,
    pub pgid: i32,
    pub leader_pid: i32,
    pub leader_start: String,
    /// "tracking" / "blocking" / "resolved"。
    pub state: String,
    pub reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// spawn 成功后的组登记输入（可安全使用的归属证据）。
#[derive(Debug, Clone, Copy)]
pub struct ManagedGroupInput<'a> {
    pub task_id: &'a str,
    pub attempt_no: u32,
    pub pgid: i32,
    pub leader_pid: i32,
    pub leader_start: &'a str,
    pub now: &'a str,
}

impl Store {
    /// spawn 成功后持久登记受管进程组（归属证据：pgid + 组长 pid 与其
    /// 启动时间）。本写入在 spawn 之后——失败不撤销启动许可，但缺少
    /// 组记录时不得自动解封（缺进程记录）。
    pub fn insert_managed_group(&self, input: ManagedGroupInput<'_>) -> Result<(), StoreError> {
        validate_id(input.task_id, "task_id")?;
        if input.attempt_no == 0 || input.pgid <= 0 || input.leader_pid <= 0 {
            return Err(StoreError::InvalidInput(
                "attempt_no/pgid/leader_pid 必须为正".into(),
            ));
        }
        require_time(input.now, "now")?;
        let result = self.lock_conn()?.execute(
            "INSERT INTO managed_groups (task_id, attempt_no, pgid, leader_pid, leader_start,
                                         state, reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'tracking', NULL, ?6, ?6)",
            params![
                input.task_id,
                i64::from(input.attempt_no),
                input.pgid,
                input.leader_pid,
                input.leader_start,
                input.now
            ],
        );
        match result {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(ce, _))
                if ce.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(StoreError::Environment(format!(
                    "attempt {}#{} 已登记受管组，不得重复登记",
                    input.task_id, input.attempt_no
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// 释放实例级待确认标记（可信证据：从未启动或已确认退出）。只有
    /// 这类证据才允许解除派发门。
    pub fn release_dispatch_gate(
        &self,
        task_id: &str,
        attempt_no: u32,
        reason: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        require_time(now, "now")?;
        let n = self.lock_conn()?.execute(
            "UPDATE dispatch_gate SET state = 'released', reason = ?1, updated_at = ?2
             WHERE task_id = ?3 AND attempt_no = ?4 AND state = 'pending'",
            params![reason, now, task_id, i64::from(attempt_no)],
        )?;
        if n == 0 {
            return Err(StoreError::Environment(format!(
                "attempt {task_id}#{attempt_no} 无待确认标记可释放"
            )));
        }
        Ok(())
    }

    /// 明确未启动（可信证据：spawn 失败，或 probe 阶段拒绝/失败且进程
    /// 已确认退出）：失败状态（SpawnFailed）、事件、待确认标记解封与
    /// 审计在**同一** IMMEDIATE 事务提交。任一步失败整体回滚——不会
    /// 出现"任务已 failed 却留下 pending 标记"的半提交。
    pub fn commit_spawn_not_started(
        &self,
        task_id: &str,
        attempt_no: u32,
        expected_revision: u64,
        evidence: &str,
        now: &str,
    ) -> Result<TransitionOutcome, StoreError> {
        validate_id(task_id, "task_id")?;
        if attempt_no == 0 || expected_revision == 0 {
            return Err(StoreError::InvalidInput(
                "attempt_no 与 expected_revision 必须为正整数".into(),
            ));
        }
        if evidence.trim().is_empty() {
            return Err(StoreError::InvalidInput("证据不能为空".into()));
        }
        require_time(now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match commit_spawn_not_started_in_tx(
            &tx,
            task_id,
            attempt_no,
            expected_revision,
            evidence,
            now,
        ) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 记录/清除待确认标记上的进程组（probe 等未进入 managed_groups 的
    /// 子进程）：pgid>0 登记归属证据；pgid=0 清除（该组已确认退出）。
    pub fn set_gate_pgid(
        &self,
        task_id: &str,
        attempt_no: u32,
        pgid: i32,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        if pgid < 0 {
            return Err(StoreError::InvalidInput("pgid 不能为负".into()));
        }
        require_time(now, "now")?;
        let n = self.lock_conn()?.execute(
            "UPDATE dispatch_gate SET pgid = ?1, updated_at = ?2
             WHERE task_id = ?3 AND attempt_no = ?4 AND state = 'pending'",
            params![pgid, now, task_id, i64::from(attempt_no)],
        )?;
        if n == 0 {
            return Err(StoreError::Environment(format!(
                "attempt {task_id}#{attempt_no} 无待确认标记可登记进程组"
            )));
        }
        Ok(())
    }

    /// 待确认标记的阻断原因（供 doctor 展示；不改状态）。
    pub fn set_gate_reason(
        &self,
        task_id: &str,
        attempt_no: u32,
        reason: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        require_time(now, "now")?;
        self.lock_conn()?.execute(
            "UPDATE dispatch_gate SET reason = ?1, updated_at = ?2
             WHERE task_id = ?3 AND attempt_no = ?4 AND state = 'pending'",
            params![reason, now, task_id, i64::from(attempt_no)],
        )?;
        Ok(())
    }

    /// 列出待确认标记（pending）。
    pub fn list_pending_gates(&self) -> Result<Vec<GateRecord>, StoreError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, attempt_no, state, pgid, reason, created_at, updated_at
             FROM dispatch_gate WHERE state = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GateRecord {
                    task_id: r.get(0)?,
                    attempt_no: r.get::<_, i64>(1)? as u32,
                    state: r.get(2)?,
                    pgid: r.get(3)?,
                    reason: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// spawn 确认事务：Spawned 状态推进（CAS）与组转 tracking 同一
    /// IMMEDIATE 事务；失败整体回滚（组保持 pending，仍阻断领取）。
    pub fn confirm_spawn_running(
        &self,
        task_id: &str,
        attempt_no: u32,
        expected_revision: u64,
        now: &str,
    ) -> Result<TransitionOutcome, StoreError> {
        validate_id(task_id, "task_id")?;
        require_time(now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match confirm_spawn_in_tx(&tx, task_id, attempt_no, expected_revision, now) {
            Ok(outcome) => {
                tx.commit()?;
                Ok(outcome)
            }
            Err(err) => {
                tx.rollback().ok();
                Err(err)
            }
        }
    }

    /// 回收未证实退出：组转 blocking——实例级派发门在领取事务内生效，
    /// 直到获得可信退出证据（组为空）或人工确认。
    pub fn mark_group_blocking(
        &self,
        task_id: &str,
        attempt_no: u32,
        reason: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        require_time(now, "now")?;
        let n = self.lock_conn()?.execute(
            "UPDATE managed_groups SET state = 'blocking', reason = ?1, updated_at = ?2
             WHERE task_id = ?3 AND attempt_no = ?4 AND state IN ('tracking','blocking')",
            params![reason, now, task_id, i64::from(attempt_no)],
        )?;
        if n == 0 {
            return Err(StoreError::Environment(format!(
                "attempt {task_id}#{attempt_no} 无 tracking/blocking 组可标记"
            )));
        }
        Ok(())
    }

    /// 可信退出证据（组为空）到达：组转 resolved，解除派发门。
    pub fn resolve_managed_group(
        &self,
        task_id: &str,
        attempt_no: u32,
        now: &str,
    ) -> Result<(), StoreError> {
        validate_id(task_id, "task_id")?;
        require_time(now, "now")?;
        let n = self.lock_conn()?.execute(
            "UPDATE managed_groups SET state = 'resolved', reason = '组退出已证实', updated_at = ?1
             WHERE task_id = ?2 AND attempt_no = ?3 AND state IN ('tracking','blocking')",
            params![now, task_id, i64::from(attempt_no)],
        )?;
        if n == 0 {
            return Err(StoreError::Environment(format!(
                "attempt {task_id}#{attempt_no} 无可解除的组记录"
            )));
        }
        Ok(())
    }

    /// 受管恢复：释放待确认标记并记录审计（attempt、证据、结果、操作者、
    /// 时间）。同一 attempt 只解封一次；重复调用返回既有审计。
    pub fn resolve_gate_with_evidence(
        &self,
        task_id: &str,
        attempt_no: u32,
        evidence: &str,
        result: &str,
        resolved_by: &str,
        now: &str,
    ) -> Result<GateResolution, StoreError> {
        validate_id(task_id, "task_id")?;
        if attempt_no == 0 {
            return Err(StoreError::InvalidInput("attempt_no 必须为正".into()));
        }
        if evidence.trim().is_empty() {
            return Err(StoreError::InvalidInput("证据不能为空".into()));
        }
        require_time(now, "now")?;
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // 已解封：返回既有审计（幂等）。
        let existing = tx
            .query_row(
                "SELECT task_id, attempt_no, evidence, result, resolved_by, resolved_at
                 FROM gate_resolutions WHERE task_id = ?1 AND attempt_no = ?2",
                params![task_id, i64::from(attempt_no)],
                |r| {
                    Ok(GateResolution {
                        task_id: r.get(0)?,
                        attempt_no: r.get::<_, i64>(1)? as u32,
                        evidence: r.get(2)?,
                        result: r.get(3)?,
                        resolved_by: r.get(4)?,
                        resolved_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        if let Some(record) = existing {
            tx.commit()?;
            return Ok(record);
        }
        // 必须存在 pending 标记才可解封。
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM dispatch_gate WHERE task_id = ?1 AND attempt_no = ?2 AND state = 'pending'",
            params![task_id, i64::from(attempt_no)],
            |r| r.get(0),
        )?;
        if pending == 0 {
            tx.rollback().ok();
            return Err(StoreError::Environment(format!(
                "attempt {task_id}#{attempt_no} 无待确认标记可解封"
            )));
        }
        tx.execute(
            "UPDATE dispatch_gate SET state = 'released', reason = ?1, updated_at = ?2
             WHERE task_id = ?3 AND attempt_no = ?4 AND state = 'pending'",
            params![evidence, now, task_id, i64::from(attempt_no)],
        )?;
        tx.execute(
            "INSERT INTO gate_resolutions (task_id, attempt_no, evidence, result, resolved_by, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_id,
                i64::from(attempt_no),
                evidence,
                result,
                resolved_by,
                now
            ],
        )?;
        tx.commit()?;
        Ok(GateResolution {
            task_id: task_id.to_owned(),
            attempt_no,
            evidence: evidence.to_owned(),
            result: result.to_owned(),
            resolved_by: resolved_by.to_owned(),
            resolved_at: now.to_owned(),
        })
    }

    /// 读取某 attempt 的恢复审计记录。
    pub fn get_gate_resolution(
        &self,
        task_id: &str,
        attempt_no: u32,
    ) -> Result<Option<GateResolution>, StoreError> {
        validate_id(task_id, "task_id")?;
        self.lock_conn()?
            .query_row(
                "SELECT task_id, attempt_no, evidence, result, resolved_by, resolved_at
                 FROM gate_resolutions WHERE task_id = ?1 AND attempt_no = ?2",
                params![task_id, i64::from(attempt_no)],
                |r| {
                    Ok(GateResolution {
                        task_id: r.get(0)?,
                        attempt_no: r.get::<_, i64>(1)? as u32,
                        evidence: r.get(2)?,
                        result: r.get(3)?,
                        resolved_by: r.get(4)?,
                        resolved_at: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 按状态列出组记录（doctor 与解除逻辑用）。
    pub fn list_managed_groups(
        &self,
        state: Option<&str>,
    ) -> Result<Vec<ManagedGroupRecord>, StoreError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT task_id, attempt_no, pgid, leader_pid, leader_start, state, reason,
                    created_at, updated_at
             FROM managed_groups WHERE (?1 IS NULL OR state = ?1) ORDER BY updated_at ASC",
        )?;
        let rows = stmt
            .query_map(params![state], |r| {
                Ok(ManagedGroupRecord {
                    task_id: r.get(0)?,
                    attempt_no: r.get::<_, i64>(1)? as u32,
                    pgid: r.get(2)?,
                    leader_pid: r.get(3)?,
                    leader_start: r.get(4)?,
                    state: r.get(5)?,
                    reason: r.get(6)?,
                    created_at: r.get(7)?,
                    updated_at: r.get(8)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}
