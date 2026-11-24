//! WP3-A1 协议夹具：hello v1、plan/submit/status 最小 DTO（14 代表性
//! 样本）、非法 UTF-8、未知/重复/缺失字段、版本不符与 256 KiB 上限。

use relay_protocol::{
    decode_frame, encode_request, Frame, PlanRequest, ProtocolError, StatusRequest, SubmitRequest,
    MAX_FRAME_BYTES,
};
use serde_json::json;

/// 14 的 relay_plan 代表性样本（04 必填字段 + 可选 base_oid）。
const PLAN_SAMPLE: &str = r#"{
    "project_id": "proj_f6a2",
    "goal": "修复块计数在空输入时的返回值",
    "brief": "仅修改 src/count.rs，并在现有用例中补空输入覆盖。",
    "write_scope": ["src/count.rs", "tests/count.rs"],
    "profile_id": "prof_claude_minimax",
    "verification_rule_ids": ["cargo_test"],
    "base_oid": "0123456789abcdef0123456789abcdef01234567"
}"#;

fn hello_frame() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "hello",
        "params": {"version": 1},
    }))
    .unwrap()
}

#[test]
fn hello_v1_decodes_and_wrong_version_is_rejected() {
    let frame = decode_frame(&hello_frame()).unwrap();
    match frame {
        Frame::Request { method, params, .. } => {
            assert_eq!(method, "hello");
            assert_eq!(params.unwrap().get("version"), Some(&json!(1)));
        }
        other => panic!("应为请求帧: {other:?}"),
    }

    let wrong = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "hello",
        "params": {"version": 2},
    }))
    .unwrap();
    let err = decode_frame(&wrong).unwrap_err();
    assert!(
        matches!(err, ProtocolError::UnsupportedVersion { .. }),
        "{err:?}"
    );
    assert_eq!(err.code(), "UNSUPPORTED_VERSION");
}

#[test]
fn duplicate_keys_are_rejected_at_any_level() {
    // 顶层重复。
    let err =
        decode_frame(br#"{"jsonrpc":"2.0","id":1,"id":1,"method":"hello","params":{"version":1}}"#)
            .unwrap_err();
    assert!(matches!(err, ProtocolError::DuplicateKey { .. }), "{err:?}");
    // 嵌套对象重复。
    let err = decode_frame(
        br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1,"version":1}}"#,
    )
    .unwrap_err();
    assert!(matches!(err, ProtocolError::DuplicateKey { .. }), "{err:?}");
    // 转义等价键：同一对象内 "\u0076ersion" 解码后与 "version" 相同 → 拒绝。
    let err = decode_frame(
        br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2,"\u0076ersion":1}}"#,
    )
    .unwrap_err();
    assert!(
        matches!(&err, ProtocolError::DuplicateKey { key } if key == "version"),
        "{err:?}"
    );
    // 同名键在不同对象层级合法（各自对象内部无重复）。
    let ok = decode_frame(br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1}}"#)
        .unwrap();
    match ok {
        Frame::Request { method, .. } => assert_eq!(method, "hello"),
        other => panic!("应为请求帧: {other:?}"),
    }
}

#[test]
fn unknown_and_missing_fields_are_rejected() {
    // 信封未知字段。
    let err = decode_frame(
        br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1},"extra":true}"#,
    )
    .unwrap_err();
    assert!(matches!(err, ProtocolError::UnknownField { .. }), "{err:?}");
    // hello 缺 version。
    let err = decode_frame(br#"{"jsonrpc":"2.0","id":1,"method":"hello"}"#).unwrap_err();
    assert!(matches!(err, ProtocolError::MissingField { .. }), "{err:?}");
    // 缺 jsonrpc。
    let err = decode_frame(br#"{"id":1,"method":"hello","params":{"version":1}}"#).unwrap_err();
    assert!(matches!(err, ProtocolError::MissingField { .. }), "{err:?}");
}

#[test]
fn invalid_utf8_and_wrong_jsonrpc_version_are_rejected() {
    // 非 UTF-8 帧。
    let err = decode_frame(&[0xFF, 0xFE, b'{', b'}']).unwrap_err();
    assert!(matches!(err, ProtocolError::InvalidUtf8), "{err:?}");
    // jsonrpc 版本 1.0 → 明确拒绝。
    let wrong = serde_json::to_vec(&json!({
        "jsonrpc": "1.0",
        "id": 1,
        "method": "hello",
        "params": {"version": 1},
    }))
    .unwrap();
    let err = decode_frame(&wrong).unwrap_err();
    assert!(
        matches!(err, ProtocolError::UnsupportedVersion { .. }),
        "{err:?}"
    );
}

#[test]
fn frame_limit_is_enforced_before_decode() {
    // 上限 + 1 字节 → FrameTooLarge（解码前检查，内容再合法也拒绝）。
    let mut oversized = hello_frame();
    oversized.extend(std::iter::repeat_n(
        b' ',
        MAX_FRAME_BYTES - oversized.len() + 1,
    ));
    assert!(oversized.len() > MAX_FRAME_BYTES);
    let err = decode_frame(&oversized).unwrap_err();
    assert!(
        matches!(err, ProtocolError::FrameTooLarge { size } if size > MAX_FRAME_BYTES),
        "{err:?}"
    );
    // 恰好 256 KiB 的合法 hello：通过上限检查并成功解码。填充放在
    // 信封 id（长字符串合法，hello 参数不允许未知字段）。
    let base = r#"{"jsonrpc":"2.0","id":"","method":"hello","params":{"version":1}}"#.to_owned();
    let pad = MAX_FRAME_BYTES - base.len();
    let long_id = "p".repeat(pad);
    let exact = format!(
        r#"{{"jsonrpc":"2.0","id":"{long_id}","method":"hello","params":{{"version":1}}}}"#
    )
    .into_bytes();
    assert_eq!(exact.len(), MAX_FRAME_BYTES, "边界帧应为恰好 256 KiB");
    assert!(decode_frame(&exact).is_ok(), "恰好上限的合法帧应通过");
}

#[test]
fn plan_request_from_spec_sample_parses_and_enforces_limits() {
    let params: serde_json::Value = serde_json::from_str(PLAN_SAMPLE).unwrap();
    let request = PlanRequest::from_params(&params).unwrap();
    assert_eq!(request.project_id, "proj_f6a2");
    assert_eq!(request.write_scope, vec!["src/count.rs", "tests/count.rs"]);
    assert_eq!(request.verification_rule_ids, vec!["cargo_test"]);
    assert_eq!(
        request.base_oid.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );

    // 未知字段拒绝。
    let with_extra = serde_json::json!({
        "project_id": "proj_f6a2",
        "goal": "g",
        "brief": "b",
        "write_scope": [],
        "profile_id": "prof",
        "verification_rule_ids": [],
        "extra_field": 1,
    });
    let err = PlanRequest::from_params(&with_extra).unwrap_err();
    assert!(matches!(err, ProtocolError::UnknownField { .. }), "{err:?}");

    // 缺失字段拒绝。
    let missing = serde_json::json!({"project_id": "proj_f6a2"});
    let err = PlanRequest::from_params(&missing).unwrap_err();
    assert!(matches!(err, ProtocolError::MissingField { .. }), "{err:?}");

    // goal 超 1 KiB。
    let long_goal = "长".repeat(600); // 1800 字节
    let over = serde_json::json!({
        "project_id": "proj_f6a2",
        "goal": long_goal,
        "brief": "b",
        "write_scope": [],
        "profile_id": "prof",
        "verification_rule_ids": [],
    });
    let err = PlanRequest::from_params(&over).unwrap_err();
    assert!(matches!(err, ProtocolError::Constraint { .. }), "{err:?}");

    // rule IDs 重复 → 约束拒绝（04：去重）。
    let dup_rules = serde_json::json!({
        "project_id": "proj_f6a2",
        "goal": "g",
        "brief": "b",
        "write_scope": [],
        "profile_id": "prof",
        "verification_rule_ids": ["cargo_test", "cargo_test"],
    });
    let err = PlanRequest::from_params(&dup_rules).unwrap_err();
    assert!(matches!(err, ProtocolError::Constraint { .. }), "{err:?}");
}

#[test]
fn submit_and_status_requests_parse_minimally() {
    let params =
        json!({"plan_id": "plan_ab12", "request_id": "550e8400-e29b-41d4-a716-446655440000"});
    let request = SubmitRequest::from_params(&params).unwrap();
    assert_eq!(request.plan_id, "plan_ab12");
    let err = SubmitRequest::from_params(&json!({"plan_id": ""})).unwrap_err();
    assert!(matches!(err, ProtocolError::MissingField { .. }), "{err:?}");
    let err = SubmitRequest::from_params(&json!({"other": 1})).unwrap_err();
    assert!(matches!(err, ProtocolError::UnknownField { .. }), "{err:?}");

    let params = json!({"task_id": "task_b442"});
    let request = StatusRequest::from_params(&params).unwrap();
    assert_eq!(request.task_id, "task_b442");
    let err = StatusRequest::from_params(&json!({})).unwrap_err();
    assert!(matches!(err, ProtocolError::MissingField { .. }), "{err:?}");
    let err = StatusRequest::from_params(&json!({"other": 1})).unwrap_err();
    assert!(matches!(err, ProtocolError::UnknownField { .. }), "{err:?}");
}

#[test]
fn stable_error_structure_serializes_with_schema_version() {
    let error = relay_protocol::RpcError::new("INVALID_ARGUMENT", "字段超限", "fix_request");
    let json = error.to_json();
    assert!(json.contains("\"schema_version\":1"));
    assert!(json.contains("\"code\":\"INVALID_ARGUMENT\""));
    assert!(json.contains("\"action\":\"fix_request\""));
    // 错误体嵌入 JSON-RPC 响应信封后可读回稳定错误结构。
    let frame = r#"{"jsonrpc":"2.0","id":9,"schema_version":1,"error":{"code":"INVALID_ARGUMENT","message":"字段超限","action":"fix_request"}}"#.to_owned();
    match decode_frame(frame.as_bytes()).unwrap() {
        Frame::ErrorResponse { error, .. } => {
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(error.action, "fix_request");
        }
        other => panic!("应为错误响应帧: {other:?}"),
    }
    // schema_version 不符拒绝。
    let wrong_version = r#"{"jsonrpc":"2.0","id":9,"schema_version":2,"error":{"code":"X","message":"m","action":"a"}}"#.to_owned();
    let err = decode_frame(wrong_version.as_bytes()).unwrap_err();
    assert!(
        matches!(err, ProtocolError::UnsupportedVersion { .. }),
        "{err:?}"
    );
}

#[test]
fn encode_request_produces_decodable_frame() {
    let id = json!(7);
    let bytes =
        encode_request("relay_status", &id, Some(&json!({"task_id": "task_b442"}))).unwrap();
    match decode_frame(&bytes).unwrap() {
        Frame::Request { method, params, .. } => {
            assert_eq!(method, "relay_status");
            assert_eq!(params.unwrap(), json!({"task_id": "task_b442"}));
        }
        other => panic!("应为请求帧: {other:?}"),
    }
}

#[test]
fn encode_response_enforces_cap_at_serialization_stage() {
    // 超限 result：序列化阶段即拒绝（FrameTooLarge），不发半帧。
    let huge = json!({"blob": "x".repeat(MAX_FRAME_BYTES)});
    let err = relay_protocol::encode_response(&json!(1), &huge).unwrap_err();
    assert!(
        matches!(err, relay_protocol::ProtocolError::FrameTooLarge { .. }),
        "{err:?}"
    );
    // 恰好上限内：编码成功且可解码回响应帧。
    let ok = relay_protocol::encode_response(&json!(7), &json!({"state": "ready"})).unwrap();
    match relay_protocol::decode_frame(&ok).unwrap() {
        relay_protocol::Frame::Response { id, result } => {
            assert_eq!(id, json!(7));
            assert_eq!(result, json!({"state": "ready"}));
        }
        other => panic!("应为响应帧: {other:?}"),
    }
}
