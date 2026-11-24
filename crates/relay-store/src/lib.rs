//! Agent Relay WP1-A / WP1-B / WP1-C1 / WP1-C2 / WP1-D1：SQLite 持久化、
//! 提交去重、单阶段状态 CAS、启动恢复、按入队顺序领取、schema v2 迁移、
//! 持久化启动栅栏与显式重试。
//!
//! 范围：最小建库（当前 schema v2：栅栏表 launch_fences）、计划记录、
//! `relay_submit` 的原子提交与幂等去重、按 ID 查回任务与请求结果、单阶段
//! 状态变更的持久化 CAS（revision 预期值比较 + `relay_core::transition`
//! 决定新状态）、启动时的显式恢复（`relay_core::recover` 分类 +
//! MarkInterrupted 批量写入）、`claim_next` 按持久化入队顺序原子领取、
//! v1 → v2 显式离线迁移（一致备份 + 校验 + 事务内 DDL）、`launch_fence`
//! 持久化启动栅栏（仅 preparing → launching，同一 attempt 一次许可，代号
//! 全局唯一），以及 `retry` 显式重试（复用 may_retry，新 plan/新 attempt，
//! 旧 attempt 与快照不变）。预览生成、IPC、MCP、worker、实例锁、进程
//! 监督、启动执行、自动重试与对外 API 属后续任务包。状态枚举复用
//! [`relay_core::State`]，WP0 状态机未做任何改动。
//!
//! 规格依据：03（调度、状态机与存储契约）、04（协议与错误契约）、
//! 08（验收矩阵）、09（备份与迁移运行手册）、14（契约样本与数据定义）。
//! SQLite 按 03 配置：WAL、外键、忙等待上限、`synchronous=FULL`；schema
//! 单向迁移：未知更高版本拒绝打开，v1 须经 [`migrate_v1_to_v2`] 显式迁移，
//! open 不做静默升级。

#![forbid(unsafe_code)]

mod error;
mod store;

pub use error::StoreError;
pub use store::{
    migrate_v1_to_v2, migrate_v2_to_v3, migrate_v3_to_v4, migrate_v4_to_v5, migrate_v5_to_v6,
    migrate_v6_to_v7, migrate_v7_to_v8, migrate_v8_to_v9, sqlite_version, ArtifactRecord,
    ArtifactRegisterInput, CancelInput, EventRecord, GateRecord, GateResolution, LaunchFenceInput,
    LaunchFenceOutcome, LaunchFenceRecord, ManagedGroupInput, ManagedGroupRecord, PlanInput,
    PlanRecord, RecoveryRecord, RequestResult, RetryInput, ReviewInput, StageFinish, StageKey,
    StageKind, StageRunRecord, Store, SubmitInput, SubmitOutcome, TaskPage, TaskSummary,
    TransitionInput, TransitionOutcome, OP_CANCEL, OP_RETRY, OP_REVIEW, OP_SUBMIT, SCHEMA_VERSION,
};
