//! WP3-A2b 会话契约测试：分段输入、单次大块输入、上限边界、无换行
//! 超限、错误握手零业务调用、断帧、输出限长、hello 确认帧与每连接
//! 单条业务请求。固定只读 status 替身作为处理器。

use relay_protocol::{decode_frame, Frame, RequestHandler, Session, SessionError, MAX_FRAME_BYTES};
use serde_json::{json, Value};

/// 固定只读 status 替身：只应答 relay_status，返回固定 JSON；记录
/// 调用次数供零调用/单次调用断言。
#[derive(Default)]
struct StatusStub {
    calls: usize,
}

impl RequestHandler for StatusStub {
    fn handle(&mut self, method: &str, _params: Option<&Value>, _id: &Value) -> Option<Value> {
        self.calls += 1;
        match method {
            "relay_status" => Some(json!({
                "schema_version": 1,
                "task_id": "task_stub-1",
                "state": "interrupted",
            })),
            _ => None,
        }
    }
}

/// 返回超大响应的替身（输出限长回归）。
#[derive(Default)]
struct HugeResultStub;

impl RequestHandler for HugeResultStub {
    fn handle(&mut self, _method: &str, _params: Option<&Value>, _id: &Value) -> Option<Value> {
        Some(json!({ "blob": "x".repeat(MAX_FRAME_BYTES + 1) }))
    }
}

fn hello_bytes() -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "hello",
        "params": {"version": 1},
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}

fn status_request_bytes(id: i64) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "relay_status",
        "params": {"task_id": "task_stub-1"},
    }))
    .unwrap();
    bytes.push(b'\n');
    bytes
}

/// 断言输出解码为响应帧并返回 (id, result)。
fn expect_response(bytes: &[u8]) -> (Value, Value) {
    match decode_frame(bytes).unwrap() {
        Frame::Response { id, result } => (id, result),
        other => panic!("应为响应帧: {other:?}"),
    }
}

#[test]
fn segmented_input_handshakes_then_status_round_trips() {
    // 握手帧逐字节分段推入：凑齐完整帧才解码；hello 确认帧在最后一
    // 字节到达后整体输出（不发半帧）。
    let mut session = Session::new(StatusStub::default());
    let hello = hello_bytes();
    for byte in &hello[..hello.len() - 1] {
        let out = session.push(std::slice::from_ref(byte)).unwrap();
        assert!(out.is_empty(), "握手确认帧不得分片输出");
    }
    let out = session.push(&hello[hello.len() - 1..]).unwrap();
    let (id, result) = expect_response(&out);
    assert_eq!(id, json!(1));
    assert_eq!(result, json!({"version": 1}));

    // hello 只完成握手，不调用处理器。
    assert_eq!(session.handler().calls, 0);
    let out = session.push(&status_request_bytes(7)).unwrap();
    let (id, result) = expect_response(&out);
    assert_eq!(id, json!(7));
    assert_eq!(
        result,
        json!({"schema_version": 1, "task_id": "task_stub-1", "state": "interrupted"})
    );
    assert!(session.is_closed());
    // EOF 时缓冲为空：正常结束。
    session.finish().unwrap();
}

#[test]
fn second_business_request_is_rejected() {
    // 回复后关闭：后续输入报 SessionClosed，处理器只被调用一次。
    let mut session = Session::new(StatusStub::default());
    session.push(&hello_bytes()).unwrap();
    session.push(&status_request_bytes(7)).unwrap();
    assert_eq!(session.handler().calls, 1);
    let err = session.push(&status_request_bytes(8)).unwrap_err();
    assert!(matches!(err, SessionError::SessionClosed), "{err:?}");
    assert_eq!(session.handler().calls, 1);
    assert!(session.is_failed());
    assert!(matches!(session.finish(), Err(SessionError::AlreadyFailed)));

    // 同一帧流内流水线第二条业务请求：整次输入拒绝（无半帧输出），
    // 处理器仍只调用一次。
    let mut session = Session::new(StatusStub::default());
    let mut chunk = hello_bytes();
    chunk.extend(status_request_bytes(7));
    chunk.extend(status_request_bytes(8));
    let err = session.push(&chunk).unwrap_err();
    assert!(matches!(err, SessionError::SessionClosed), "{err:?}");
    assert_eq!(session.handler().calls, 1);
}

#[test]
fn single_large_push_processes_both_frames() {
    // 单次大块输入：hello + 业务请求一次推入，确认帧与响应按序整体
    // 输出。
    let mut session = Session::new(StatusStub::default());
    let mut chunk = hello_bytes();
    chunk.extend(status_request_bytes(7));
    let out = session.push(&chunk).unwrap();
    let hello_pos = out.iter().position(|&b| b == b'\n').unwrap();
    let (id, result) = expect_response(&out[..hello_pos]);
    assert_eq!(id, json!(1));
    assert_eq!(result, json!({"version": 1}));
    let (id, _) = expect_response(&out[hello_pos + 1..]);
    assert_eq!(id, json!(7));
    assert!(session.is_closed());
    session.finish().unwrap();
}

#[test]
fn exact_limit_handshake_frame_passes_boundary() {
    // 恰好 256 KiB 的合法 hello 行（含换行共 MAX+1 字节）：通过。
    let base = r#"{"jsonrpc":"2.0","id":"","method":"hello","params":{"version":1}}"#;
    let pad = MAX_FRAME_BYTES - base.len();
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":"{}","method":"hello","params":{{"version":1}}}}"#,
        "p".repeat(pad)
    );
    assert_eq!(line.len(), MAX_FRAME_BYTES);
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    let mut session = Session::new(StatusStub::default());
    let out = session.push(&bytes).unwrap();
    let (_, result) = expect_response(&out);
    assert_eq!(result, json!({"version": 1}));
    assert!(session.finish().is_ok());
}

#[test]
fn no_newline_over_limit_is_rejected_immediately() {
    // 无换行持续超限：不等下一帧，立即拒绝且不复制整块数据。
    let mut session = Session::new(StatusStub::default());
    let err = session.push(&vec![b'{'; MAX_FRAME_BYTES + 1]).unwrap_err();
    assert!(
        matches!(err, SessionError::FrameTooLarge { size } if size == MAX_FRAME_BYTES + 1),
        "{err:?}"
    );
    assert!(session.is_failed());
    // 失败会话拒绝继续输入，处理器零调用。
    assert!(matches!(
        session.push(b"x\n"),
        Err(SessionError::AlreadyFailed)
    ));
    assert!(matches!(session.finish(), Err(SessionError::AlreadyFailed)));
    assert_eq!(session.handler().calls, 0);
}

#[test]
fn buffered_prefix_plus_chunk_over_limit_is_rejected() {
    // 缓冲前缀 + 新块合计超限：追加缓冲之前拒绝，超限新块不进缓冲。
    let mut session = Session::new(StatusStub::default());
    let first = vec![b'x'; MAX_FRAME_BYTES - 10];
    session.push(&first).unwrap();
    let err = session.push(&[b'y'; 11]).unwrap_err();
    assert!(
        matches!(err, SessionError::FrameTooLarge { size } if size == MAX_FRAME_BYTES + 1),
        "{err:?}"
    );
    assert!(session.is_failed());
    // 恰好到上限的追加仍被缓冲（等待换行或断帧判定）。
    let mut session = Session::new(StatusStub::default());
    let first = vec![b'x'; MAX_FRAME_BYTES - 10];
    session.push(&first).unwrap();
    session.push(&[b'y'; 10]).unwrap();
    assert!(matches!(
        session.finish(),
        Err(SessionError::IncompleteFrame { .. })
    ));
}

#[test]
fn over_limit_single_line_with_newline_is_rejected() {
    // 单行（含换行）超过 256 KiB → 帧级拒绝。
    let mut session = Session::new(StatusStub::default());
    let mut bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
    bytes.push(b'\n');
    let err = session.push(&bytes).unwrap_err();
    assert!(
        matches!(err, SessionError::FrameTooLarge { size } if size == MAX_FRAME_BYTES + 1),
        "{err:?}"
    );
    assert!(session.is_failed());
    assert_eq!(session.handler().calls, 0);
}

#[test]
fn bad_handshake_rejects_without_calling_handler() {
    // 首帧不是 hello（业务请求）→ 拒绝。
    let mut session = Session::new(StatusStub::default());
    let err = session.push(&status_request_bytes(7)).unwrap_err();
    assert!(
        matches!(err, SessionError::HandshakeRejected { .. }),
        "{err:?}"
    );
    assert!(session.is_failed());
    assert!(matches!(session.finish(), Err(SessionError::AlreadyFailed)));
    assert_eq!(session.handler().calls, 0);

    // hello 版本不符 → 拒绝。
    let mut session = Session::new(StatusStub::default());
    let mut wrong = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"version": 2},
    }))
    .unwrap();
    wrong.push(b'\n');
    let err = session.push(&wrong).unwrap_err();
    assert!(
        matches!(err, SessionError::HandshakeRejected { .. }),
        "{err:?}"
    );

    // hello 带重复键 → 拒绝。
    let mut session = Session::new(StatusStub::default());
    let mut dup =
        br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1,"version":1}}"#.to_vec();
    dup.push(b'\n');
    let err = session.push(&dup).unwrap_err();
    assert!(
        matches!(err, SessionError::HandshakeRejected { .. }),
        "{err:?}"
    );

    // 首帧是响应帧 → 拒绝。
    let mut session = Session::new(StatusStub::default());
    let mut response = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_vec();
    response.push(b'\n');
    let err = session.push(&response).unwrap_err();
    assert!(
        matches!(err, SessionError::HandshakeRejected { .. }),
        "{err:?}"
    );
}

#[test]
fn incomplete_frame_at_eof_is_rejected() {
    // 握手后推入不完整帧，EOF 时缓冲残留 → 断帧拒绝，零调用。
    let mut session = Session::new(StatusStub::default());
    session.push(&hello_bytes()).unwrap();
    let full = status_request_bytes(7);
    session.push(&full[..full.len() / 2]).unwrap();
    let err = session.finish().unwrap_err();
    assert!(
        matches!(err, SessionError::IncompleteFrame { size } if size == full.len() / 2),
        "{err:?}"
    );
    assert!(session.is_failed());
    assert_eq!(session.handler().calls, 0);

    // 对比：无缓冲残留的 EOF 正常结束。
    let mut session = Session::new(StatusStub::default());
    session.push(&hello_bytes()).unwrap();
    session.finish().unwrap();

    // 完整请求 + EOF：会话已回复并关闭，正常结束。
    let mut session = Session::new(StatusStub::default());
    session.push(&hello_bytes()).unwrap();
    let out = session.push(&status_request_bytes(7)).unwrap();
    assert!(!out.is_empty());
    session.finish().unwrap();
}

#[test]
fn oversized_output_is_not_emitted() {
    // 替身返回超过 256 KiB 的 result：响应限长拒绝，不输出任何字节。
    let mut session = Session::new(HugeResultStub);
    session.push(&hello_bytes()).unwrap();
    let err = session.push(&status_request_bytes(7)).unwrap_err();
    assert!(
        matches!(err, SessionError::ResponseTooLarge { size } if size > MAX_FRAME_BYTES),
        "{err:?}"
    );
    assert!(session.is_failed());
}
