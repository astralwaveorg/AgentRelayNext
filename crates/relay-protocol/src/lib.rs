//! Agent Relay WP3-A1：本机协议基础（04、14、17）。
//!
//! - [`frame`]：256 KiB 字节上限（解码前检查）、UTF-8、重复键预扫描、
//!   JSON-RPC 2.0 信封与 hello v1 校验。
//! - [`dto`]：relay_plan / relay_submit / relay_status 的最小 DTO 与
//!   04 字段约束（长度、条数、去重）。
//! - [`session`]：内存流上的限长分帧与会话握手（A2b 前半段）。
//! - [`error`]：可区分的协议错误与 04 稳定错误结构。
//!
//! 本 crate 不接 socket、数据库、用户配置或 MCP SDK，不做真实执行；
//! serde_json 不报重复键，故在解码前做原始文本预扫描。

#![forbid(unsafe_code)]

pub mod dto;
pub mod error;
pub mod frame;
pub mod session;

pub use dto::{
    ArtifactRequest, CancelRequest, EventsRequest, ListRequest, PlanRequest, RetryRequest,
    ReviewRequest, StatusRequest, StatusResult, SubmitRequest, SubmitResult, TaskRef,
    MAX_BRIEF_BYTES, MAX_GOAL_BYTES, MAX_RULE_IDS, MAX_WRITE_SCOPE_ENTRIES,
    MAX_WRITE_SCOPE_ENTRY_BYTES,
};
pub use error::{map_serde, ProtocolError, RpcError};
pub use frame::{
    decode_frame, encode_request, encode_response, Frame, WireError, HELLO_VERSION,
    JSONRPC_VERSION, MAX_FRAME_BYTES,
};
pub use session::{RequestHandler, Session, SessionError};

/// JSON 字符串转义（引号、反斜杠与控制字符），供错误结构与手写
/// JSON 片段共用。
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
