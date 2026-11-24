//! Agent Relay WP0：attempt 状态机的纯函数契约。
//!
//! 只提供 [`transition`]、[`recover`]、[`may_retry`] 三个纯函数及相关类型。
//! SQLite 存储、事件落库、MCP、子进程与 UI 属于后续任务包，不在本 crate 内。
//! 状态与事件的合法边、恢复分类和重试条件以
//! `agent-relay-design-docs/spec/12-glm-pilot.md` 与
//! `spec/03-scheduler-storage.md` 为准。

#![forbid(unsafe_code)]

mod state;

pub use state::{may_retry, recover, transition, Event, InvalidTransition, State};
