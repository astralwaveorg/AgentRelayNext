//! 帧解码：字节上限在解码前检查，重复键预扫描，JSON-RPC 2.0 信封与
//! hello v1 校验。
//!
//! 依据 04：本机 RPC 为 UTF-8 单行 JSON-RPC 2.0，请求与单个响应限
//! 256 KiB，逐条限长；第一帧为 hello（携版本 1），版本不匹配明确拒绝；
//! 拒绝未知字段与重复键。解码前先查字节上限，再做 UTF-8 与重复键
//! 预扫描，最后交 serde_json 结构化解析。

use serde::Deserialize;
use serde_json::Value;

use crate::error::{map_serde, ProtocolError};

/// 帧字节上限（04：请求和单个响应限 256 KiB）。
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// JSON-RPC 版本常量。
pub const JSONRPC_VERSION: &str = "2.0";

/// hello 版本常量。
pub const HELLO_VERSION: u32 = 1;

/// 解码后的帧。请求携方法名与参数（原始 JSON 值，由调用方按方法解析
/// 为类型化 DTO）；响应携 result 或稳定错误结构。
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// 请求帧（含 hello）。
    Request {
        method: String,
        id: Value,
        params: Option<Value>,
    },
    /// 成功响应帧。
    Response { id: Value, result: Value },
    /// 错误响应帧（04 稳定错误结构）。
    ErrorResponse { id: Value, error: WireError },
}

/// 04 稳定错误结构体（解码侧；schema_version 在信封层）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    pub code: String,
    pub message: String,
    pub action: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    jsonrpc: String,
    #[serde(default)]
    schema_version: Option<u32>,
    id: Value,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<WireError>,
}

/// 解码一帧：字节上限 → UTF-8 → 重复键预扫描 → JSON-RPC 2.0 信封 →
/// hello v1 校验。任何失败都不产生部分结果。
pub fn decode_frame(bytes: &[u8]) -> Result<Frame, ProtocolError> {
    // 1. 字节上限在解码前检查。
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge { size: bytes.len() });
    }
    // 2. UTF-8。
    let text = std::str::from_utf8(bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
    // 3. 重复键预扫描（serde_json 不会报重复键）。
    scan_duplicate_keys(text)?;
    // 4. 结构化解析。
    let envelope: Envelope = serde_json::from_str(text).map_err(|e| map_serde(&e))?;
    if envelope.jsonrpc != JSONRPC_VERSION {
        return Err(ProtocolError::UnsupportedVersion {
            detail: format!(
                "jsonrpc {} 不受支持，本实现仅支持 {JSONRPC_VERSION}",
                envelope.jsonrpc
            ),
        });
    }
    match (&envelope.method, &envelope.error, &envelope.result) {
        (Some(method), None, None) => {
            // 请求帧。hello 携版本 1：版本不符明确拒绝；hello 参数拒绝
            // 未知字段（只允许 version）。
            if method == "hello" {
                let params =
                    envelope
                        .params
                        .as_ref()
                        .ok_or_else(|| ProtocolError::MissingField {
                            field: "version".into(),
                        })?;
                let params_obj = params
                    .as_object()
                    .ok_or_else(|| ProtocolError::Constraint {
                        detail: "hello params 必须是对象".into(),
                    })?;
                for key in params_obj.keys() {
                    if key != "version" {
                        return Err(ProtocolError::UnknownField { field: key.clone() });
                    }
                }
                let version = params_obj.get("version").and_then(Value::as_u64);
                match version {
                    Some(v) if v == HELLO_VERSION as u64 => {}
                    Some(v) => {
                        return Err(ProtocolError::UnsupportedVersion {
                            detail: format!("hello 版本 {v} 不受支持，需要 {HELLO_VERSION}"),
                        });
                    }
                    None => {
                        return Err(ProtocolError::MissingField {
                            field: "version".into(),
                        });
                    }
                }
            }
            Ok(Frame::Request {
                method: method.clone(),
                id: envelope.id.clone(),
                params: envelope.params.clone(),
            })
        }
        (None, Some(error), None) => {
            match envelope.schema_version {
                Some(1) | None => {}
                Some(v) => {
                    return Err(ProtocolError::UnsupportedVersion {
                        detail: format!("错误结构 schema_version {v}"),
                    });
                }
            }
            Ok(Frame::ErrorResponse {
                id: envelope.id.clone(),
                error: error.clone(),
            })
        }
        (None, None, Some(result)) => Ok(Frame::Response {
            id: envelope.id.clone(),
            result: result.clone(),
        }),
        _ => Err(ProtocolError::InvalidJson {
            detail: "帧必须恰好包含 method（请求）或 result/error（响应）之一".into(),
        }),
    }
}

/// 序列化阶段限长写入器：累计字节一过上限即报错，不再继续缓冲
/// （超限响应不产生完整或部分缓冲，不发半帧）。
struct CappedWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + chunk.len() > self.cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "响应超过 256 KiB 上限",
            ));
        }
        self.buf.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_capped(
    write: impl FnOnce(&mut CappedWriter) -> serde_json::Result<()>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut writer = CappedWriter {
        buf: Vec::with_capacity(4 * 1024),
        cap: MAX_FRAME_BYTES,
    };
    match write(&mut writer) {
        Ok(()) => Ok(writer.buf),
        Err(err) => {
            if err.is_io() {
                let detail = err.to_string();
                if detail.contains("响应超过 256 KiB 上限") {
                    return Err(ProtocolError::FrameTooLarge {
                        size: MAX_FRAME_BYTES + 1,
                    });
                }
                return Err(ProtocolError::InvalidJson { detail });
            }
            Err(ProtocolError::InvalidJson {
                detail: err.to_string(),
            })
        }
    }
}

#[derive(serde::Serialize)]
struct ResponseFrame<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    result: &'a Value,
}

#[derive(serde::Serialize)]
struct RequestFrame<'a> {
    jsonrpc: &'static str,
    id: &'a Value,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

/// 编码成功响应帧：id 与 result 经 serde_json 直接写入限长缓冲——字节
/// 上限在序列化阶段生效（超限不产生完整或部分缓冲，不发半帧）。
pub fn encode_response(id: &Value, result: &Value) -> Result<Vec<u8>, ProtocolError> {
    serialize_capped(|w| {
        serde_json::to_writer(
            w,
            &ResponseFrame {
                jsonrpc: JSONRPC_VERSION,
                id,
                result,
            },
        )
    })
}

/// 编码请求帧：method/id/params 经 serde_json 直接写入限长缓冲（正确
/// 转义；字节上限在序列化阶段生效）。
pub fn encode_request(
    method: &str,
    id: &Value,
    params: Option<&Value>,
) -> Result<Vec<u8>, ProtocolError> {
    serialize_capped(|w| {
        serde_json::to_writer(
            w,
            &RequestFrame {
                jsonrpc: JSONRPC_VERSION,
                id,
                method,
                params,
            },
        )
    })
}

/// Parse an already bounded external JSON value without duplicate keys.
pub fn decode_json_value(text: &str) -> Result<Value, ProtocolError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge { size: text.len() });
    }
    scan_duplicate_keys(text)?;
    serde_json::from_str(text).map_err(|err| crate::map_serde(&err))
}

fn scan_duplicate_keys(text: &str) -> Result<(), ProtocolError> {
    #[derive(Clone)]
    enum Ctx {
        Object(Vec<String>),
        Array,
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Ctx> = vec![Ctx::Object(Vec::new())]; // 虚拟根
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // 字符串（含转义），读到闭合引号。
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    // 未闭合字符串：交由 serde_json 报语法错误。
                    return Ok(());
                }
                // 解码出实际键值（\u 转义与代理对展开），用于判重。
                let decoded = decode_json_string_raw(&text[start..j])?;
                i = j + 1;
                // 跳过空白判断角色。
                let mut k = i;
                while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b':' {
                    // 当前对象层级中的键。
                    if let Some(Ctx::Object(keys)) = stack.last_mut() {
                        if keys.iter().any(|prev| prev == &decoded) {
                            return Err(ProtocolError::DuplicateKey { key: decoded });
                        }
                        keys.push(decoded);
                    }
                    i = k + 1;
                }
                // 否则是值字符串，无需处理。
            }
            b'{' => {
                stack.push(Ctx::Object(Vec::new()));
                i += 1;
            }
            b'[' => {
                stack.push(Ctx::Array);
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// 解码 JSON 字符串体的转义（不含外层引号）；\\uXXXX 与代理对展开。
/// 语法错误交由 serde_json 兜底，此处对无法展开的转义返回错误。
fn decode_json_string_raw(raw: &str) -> Result<String, ProtocolError> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // 转义序列：反斜杠在 i，标识符在 i+1。
            if i + 1 >= bytes.len() {
                return Err(ProtocolError::InvalidJson {
                    detail: "字符串转义不完整".into(),
                });
            }
            match bytes[i + 1] {
                b'"' => {
                    out.push('"');
                    i += 2;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                }
                b'/' => {
                    out.push('/');
                    i += 2;
                }
                b'b' => {
                    out.push('\u{0008}');
                    i += 2;
                }
                b'f' => {
                    out.push('\u{000c}');
                    i += 2;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                }
                b'u' => {
                    // \uXXXX：4 位十六进制在 i+2..i+6。
                    let hex = raw
                        .get(i + 2..i + 6)
                        .ok_or_else(|| ProtocolError::InvalidJson {
                            detail: "\\u 转义不完整".into(),
                        })?;
                    let value =
                        u32::from_str_radix(hex, 16).map_err(|_| ProtocolError::InvalidJson {
                            detail: "非法 \\u 转义".into(),
                        })?;
                    i += 6;
                    // 高代理后紧跟 \u 低代理 → 合并为实际码点。
                    if (0xD800..=0xDBFF).contains(&value)
                        && i + 6 <= bytes.len()
                        && bytes[i] == b'\\'
                        && bytes[i + 1] == b'u'
                    {
                        if let Some(hex2) = raw.get(i + 2..i + 6) {
                            if let Ok(low) = u32::from_str_radix(hex2, 16) {
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    if let Some(ch) = char::from_u32(
                                        0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00),
                                    ) {
                                        out.push(ch);
                                    }
                                    i += 6;
                                    continue;
                                }
                            }
                        }
                        // 未配对代理：按失败关闭，不产出任何字符。
                    } else if let Some(ch) = char::from_u32(value) {
                        out.push(ch);
                    }
                }
                other => {
                    return Err(ProtocolError::InvalidJson {
                        detail: format!("未知转义字符 {}", other as char),
                    })
                }
            }
        } else {
            // 常规 UTF-8 字符按字符宽度整体拷贝。
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&raw[i..i + ch_len]);
            i += ch_len;
        }
    }
    Ok(out)
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
