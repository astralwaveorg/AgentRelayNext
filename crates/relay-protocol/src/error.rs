//! 协议层错误与 04 的稳定错误结构。

use std::fmt;

/// 解码与校验错误。变体可区分，覆盖 08/17 要求的负例：
/// 未知字段、重复键、缺失字段、版本不符、字节上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// 帧超过 256 KiB 字节上限（解码前检查）。
    FrameTooLarge { size: usize },
    /// 帧不是合法 UTF-8。
    InvalidUtf8,
    /// JSON 语法或结构不符合预期。
    InvalidJson { detail: String },
    /// 同一对象层级出现重复键。
    DuplicateKey { key: String },
    /// 出现未知字段（04：拒绝未知输入字段）。
    UnknownField { field: String },
    /// 缺失必需字段。
    MissingField { field: String },
    /// 版本不符（jsonrpc 或 hello 版本）。
    UnsupportedVersion { detail: String },
    /// 字段超出 04 约束（长度、条数、去重等）。
    Constraint { detail: String },
}

impl ProtocolError {
    /// 04 错误码。
    pub fn code(&self) -> &'static str {
        match self {
            ProtocolError::FrameTooLarge { .. } => "INVALID_ARGUMENT",
            ProtocolError::InvalidUtf8 => "INVALID_ARGUMENT",
            ProtocolError::InvalidJson { .. } => "INVALID_ARGUMENT",
            ProtocolError::DuplicateKey { .. } => "INVALID_ARGUMENT",
            ProtocolError::UnknownField { .. } => "INVALID_ARGUMENT",
            ProtocolError::MissingField { .. } => "INVALID_ARGUMENT",
            ProtocolError::UnsupportedVersion { .. } => "UNSUPPORTED_VERSION",
            ProtocolError::Constraint { .. } => "INVALID_ARGUMENT",
        }
    }

    /// 04 建议的客户端动作。
    pub fn action(&self) -> &'static str {
        match self {
            ProtocolError::UnsupportedVersion { .. } => "upgrade",
            _ => "fix_request",
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::FrameTooLarge { size } => {
                write!(f, "帧 {size} 字节超过 256 KiB 上限")
            }
            ProtocolError::InvalidUtf8 => write!(f, "帧不是合法 UTF-8"),
            ProtocolError::InvalidJson { detail } => write!(f, "JSON 解析失败：{detail}"),
            ProtocolError::DuplicateKey { key } => write!(f, "重复键 {key}"),
            ProtocolError::UnknownField { field } => write!(f, "未知字段 {field}"),
            ProtocolError::MissingField { field } => write!(f, "缺失必需字段 {field}"),
            ProtocolError::UnsupportedVersion { detail } => {
                write!(f, "版本不符：{detail}")
            }
            ProtocolError::Constraint { detail } => write!(f, "字段超限：{detail}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// 把 serde_json 错误映射为可区分的协议错误。
pub fn map_serde(error: &serde_json::Error) -> ProtocolError {
    let detail = error.to_string();
    if let Some(rest) = detail.strip_prefix("missing field `") {
        if let Some(end) = rest.find('`') {
            return ProtocolError::MissingField {
                field: rest[..end].to_owned(),
            };
        }
    }
    if detail.starts_with("unknown field `") {
        let field = detail
            .strip_prefix("unknown field `")
            .and_then(|rest| rest.split('`').next())
            .unwrap_or("")
            .to_owned();
        return ProtocolError::UnknownField { field };
    }
    if detail.contains("invalid type") || detail.contains("invalid value") {
        return ProtocolError::Constraint { detail };
    }
    ProtocolError::InvalidJson { detail }
}

/// 04 的稳定业务错误结构（线格式）：`{"schema_version":1,"error":{...}}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    pub action: String,
}

impl RpcError {
    /// 构造一个 schema_version 1 的稳定错误。
    pub fn new(code: &str, message: &str, action: &str) -> RpcError {
        RpcError {
            code: code.to_owned(),
            message: message.to_owned(),
            action: action.to_owned(),
        }
    }

    /// 序列化为线格式 JSON（不含 JSON-RPC 信封）。
    pub fn to_json(&self) -> String {
        format!(
            "{{\"schema_version\":1,\"error\":{{\"code\":\"{}\",\"message\":\"{}\",\"action\":\"{}\"}}}}",
            crate::json_escape(&self.code),
            crate::json_escape(&self.message),
            crate::json_escape(&self.action)
        )
    }
}
