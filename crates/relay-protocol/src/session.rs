//! 会话层（04/17）：内存流上的限长分帧与会话握手（A2b 前半段）。
//!
//! 输入按换行分帧（04：单行 JSON-RPC 2.0，请求与响应均以 LF 分帧）：
//! 字节逐段推入有界缓冲，凑齐完整帧才解码。限长在追加缓冲之前核对
//! 剩余空间，只扫描新输入（已缓冲前缀不再重扫）；超限立即拒绝且不
//! 把整块数据复制进缓冲。单帧超过 256 KiB、无换行持续超限、EOF 时
//! 缓冲残留（断帧）、或首帧不是 hello v1 时拒绝，业务处理器零调用。
//!
//! 握手成功后向对端回发与 hello 同 ID 的 `{"version":1}` 确认帧；此后
//! 每个会话最多处理一条业务请求，回复后关闭——同一帧流内或后续输入
//! 中的第二条业务请求都报 [`SessionError::SessionClosed`]，业务处理器
//! 不会被第二次调用。响应经 [`crate::frame::encode_response`] 按上限
//! 编码后整体输出，不发半帧。
//!
//! 本模块只做内存中的帧与会话状态，不绑定真实 Unix socket、不接
//! 真实 Store、CLI/MCP 或 worker。

use serde_json::Value;

use crate::error::ProtocolError;
use crate::frame::{decode_frame, encode_response, Frame, MAX_FRAME_BYTES};

/// 业务处理器：握手成功后接收已解码的请求，返回响应 result
/// （None 表示本次无响应）。只读替身与后续真实业务都实现此 trait。
pub trait RequestHandler {
    fn handle(&mut self, method: &str, params: Option<&Value>, id: &Value) -> Option<Value>;
}

/// 会话错误。除 [`SessionError::SessionClosed`] 外，任何变体都使会话
/// 失效。
#[derive(Debug)]
pub enum SessionError {
    /// 单帧超过 256 KiB（含无换行持续超限）。
    FrameTooLarge { size: usize },
    /// EOF 时缓冲仍有未终结的字节（断帧）。
    IncompleteFrame { size: usize },
    /// 首帧不是 hello v1（方法不符、版本不符或解码失败）。
    HandshakeRejected { detail: String },
    /// 服务端会话收到响应帧等不合方向的帧。
    UnexpectedFrame { detail: String },
    /// 响应编码后超过 256 KiB（输出限长）。
    ResponseTooLarge { size: usize },
    /// 会话内解码失败（重复键、未知字段、版本不符等）。
    Protocol(ProtocolError),
    /// 唯一一条业务请求已回复、会话已关闭后仍收到输入。
    SessionClosed,
    /// 会话已失败后继续输入。
    AlreadyFailed,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::FrameTooLarge { size } => {
                write!(f, "单帧 {size} 字节超过 256 KiB 上限，会话拒绝")
            }
            SessionError::IncompleteFrame { size } => {
                write!(f, "输入结束时仍有 {size} 字节未构成完整帧（断帧）")
            }
            SessionError::HandshakeRejected { detail } => {
                write!(f, "会话握手被拒绝：{detail}")
            }
            SessionError::UnexpectedFrame { detail } => {
                write!(f, "帧方向不符：{detail}")
            }
            SessionError::ResponseTooLarge { size } => {
                write!(f, "响应 {size} 字节超过 256 KiB 上限，不输出")
            }
            SessionError::Protocol(e) => write!(f, "会话内解码失败：{e}"),
            SessionError::SessionClosed => {
                write!(f, "会话已回复唯一业务请求并关闭，拒绝后续输入")
            }
            SessionError::AlreadyFailed => write!(f, "会话已失败，拒绝继续输入"),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 等待首帧 hello v1。
    Handshake,
    /// 握手完成，等待唯一一条业务请求。
    Open,
    /// 业务请求已回复，会话正常关闭。
    Closed,
    /// 已失败：拒绝后续输入。
    Failed,
}

/// 单个会话：缓冲输入、分帧、握手与请求处理。输出以字节形式由
/// [`Session::push`] / [`Session::finish`] 返回。
#[derive(Debug)]
pub struct Session<H: RequestHandler> {
    handler: H,
    state: State,
    /// 已缓冲的未终结帧前缀（不含换行；长度恒 ≤ MAX_FRAME_BYTES）。
    buf: Vec<u8>,
}

impl<H: RequestHandler> Session<H> {
    /// 创建会话：首帧必须是 hello v1，握手前处理器不会被调用。
    pub fn new(handler: H) -> Self {
        Session {
            handler,
            state: State::Handshake,
            buf: Vec::new(),
        }
    }

    /// 推入一段输入；凑齐的完整帧逐个解码处理，返回本次产生的输出
    /// 字节（hello 确认帧与业务响应，可能为空）。
    ///
    /// 限长在追加缓冲之前核对剩余空间，只扫描新输入中的换行（已缓冲
    /// 前缀不再重扫）；无换行部分超限时立即拒绝，不复制整块数据。
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, SessionError> {
        if self.state == State::Failed {
            return Err(SessionError::AlreadyFailed);
        }
        if self.state == State::Closed {
            self.fail();
            return Err(SessionError::SessionClosed);
        }
        let mut out = Vec::new();
        let mut consumed = 0usize;
        // 只扫描新输入（bytes），buf 残留前缀已知不含换行。
        while let Some(pos) = bytes[consumed..].iter().position(|&b| b == b'\n') {
            let line_end = consumed + pos;
            let prefix = &bytes[consumed..line_end];
            // 行总长（缓冲前缀 + 本次新增）超过上限：先拒绝再拼接，
            // 超限数据不复制进缓冲。
            let line_len = self.buf.len() + prefix.len();
            if line_len > MAX_FRAME_BYTES {
                self.fail();
                return Err(SessionError::FrameTooLarge { size: line_len });
            }
            let mut line = std::mem::take(&mut self.buf);
            line.extend_from_slice(prefix);
            if let Err(err) = self.process_line(&line, &mut out) {
                self.fail();
                return Err(err);
            }
            consumed = line_end + 1;
            // 唯一业务请求已回复、会话关闭：同一帧流内的后续输入一律
            // 拒绝（第二次业务调用不发生，本次输出不发出半帧）。
            if self.state == State::Closed {
                if consumed < bytes.len() {
                    self.fail();
                    return Err(SessionError::SessionClosed);
                }
                return Ok(out);
            }
        }
        // 剩余无换行部分：先核对剩余空间再追加（超限不复制）。
        let tail = &bytes[consumed..];
        if !tail.is_empty() {
            if self.buf.len() + tail.len() > MAX_FRAME_BYTES {
                let size = self.buf.len() + tail.len();
                self.fail();
                return Err(SessionError::FrameTooLarge { size });
            }
            self.buf.extend_from_slice(tail);
        }
        Ok(out)
    }

    /// 输入结束（EOF）：缓冲必须为空，残留字节即断帧。会话已正常
    /// 关闭（回复后关闭）时为干净结束。
    pub fn finish(&mut self) -> Result<Vec<u8>, SessionError> {
        if self.state == State::Failed {
            return Err(SessionError::AlreadyFailed);
        }
        if self.state == State::Closed {
            return Ok(Vec::new());
        }
        if self.buf.is_empty() {
            return Ok(Vec::new());
        }
        let size = self.buf.len();
        self.fail();
        Err(SessionError::IncompleteFrame { size })
    }

    /// 会话是否已失败。
    pub fn is_failed(&self) -> bool {
        self.state == State::Failed
    }

    /// 会话是否已正常关闭（唯一业务请求已回复）。
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// 只读访问处理器（供调用方核对调用次数等状态）。
    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// 处理一帧（不含换行）。握手失败、协议错误、方向不符与响应超限
    /// 都返回错误，处理器不被调用（握手阶段）或不产出输出（超限）。
    fn process_line(&mut self, line: &[u8], out: &mut Vec<u8>) -> Result<(), SessionError> {
        if line.len() > MAX_FRAME_BYTES {
            return Err(SessionError::FrameTooLarge { size: line.len() });
        }
        if line.is_empty() {
            // 空行不是帧：跳过（对偶发多余换行保持稳健）。
            return Ok(());
        }
        match self.state {
            State::Handshake => {
                let frame = decode_frame(line).map_err(|e| SessionError::HandshakeRejected {
                    detail: format!("首帧解码失败：{e}"),
                })?;
                match frame {
                    Frame::Request { method, id, .. } if method == "hello" => {
                        self.state = State::Open;
                        // 握手确认：与 hello 同 ID 的 {"version":1}。
                        let confirmation =
                            encode_response(&id, &serde_json::json!({ "version": 1 })).map_err(
                                |e| match e {
                                    ProtocolError::FrameTooLarge { size } => {
                                        SessionError::ResponseTooLarge { size }
                                    }
                                    other => SessionError::Protocol(other),
                                },
                            )?;
                        // 输出与请求一致以 LF 分帧。
                        out.extend_from_slice(&confirmation);
                        out.push(b'\n');
                        Ok(())
                    }
                    Frame::Request { method, .. } => Err(SessionError::HandshakeRejected {
                        detail: format!("首帧方法 {method} 不是 hello"),
                    }),
                    _ => Err(SessionError::HandshakeRejected {
                        detail: "首帧不是 hello 请求".into(),
                    }),
                }
            }
            State::Open => {
                let frame = decode_frame(line).map_err(SessionError::Protocol)?;
                match frame {
                    Frame::Request { method, id, params } => {
                        if let Some(result) = self.handler.handle(&method, params.as_ref(), &id) {
                            let bytes = encode_response(&id, &result).map_err(|e| match e {
                                ProtocolError::FrameTooLarge { size } => {
                                    SessionError::ResponseTooLarge { size }
                                }
                                other => SessionError::Protocol(other),
                            })?;
                            // 输出与请求一致以 LF 分帧，整体输出不发半帧。
                            out.extend_from_slice(&bytes);
                            out.push(b'\n');
                        }
                        // 每连接最多一条业务请求：回复后关闭。
                        self.state = State::Closed;
                        Ok(())
                    }
                    _ => Err(SessionError::UnexpectedFrame {
                        detail: "服务端会话只接收请求帧".into(),
                    }),
                }
            }
            State::Closed | State::Failed => unreachable!("push 已拦截关闭/失败会话"),
        }
    }

    fn fail(&mut self) {
        self.state = State::Failed;
        self.buf.clear();
    }
}
