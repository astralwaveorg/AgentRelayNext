//! Candidate Claude print-mode transcript validation. Never starts a CLI or calls a model.
use serde_json::{json, Value};
use std::io::{BufRead, Read};

const FRAME: usize = 256 * 1024;
const TOTAL: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Transcript {
    initialized: bool,
    started: bool,
    ended: bool,
    end_turn: bool,
    result: Option<String>,
}

impl Transcript {
    fn message(&mut self, frame: &Value, expected: &str) -> Result<(), &'static str> {
        if self.result.is_some() {
            return Err("TRAILING_FRAME");
        }
        match frame.get("type").and_then(Value::as_str) {
            Some("system") if frame["subtype"] == "ui_invalidate" && !self.started => {}
            Some("system")
                if frame["subtype"] == "status"
                    && !self.ended
                    && frame["status"] == "requesting" => {}
            // Progress-only frames emitted while extended thinking streams.
            Some("system")
                if frame["subtype"] == "thinking_tokens" && self.started && !self.ended => {}
            Some("system") if !self.initialized && frame["subtype"] == "init" => {
                for field in ["tools", "mcp_servers", "skills", "slash_commands"] {
                    if frame[field]
                        .as_array()
                        .is_none_or(|items| !items.is_empty())
                    {
                        return Err("UNEXPECTED_TOOL_SURFACE");
                    }
                }
                if frame["permissionMode"] != "dontAsk"
                    || frame["plugins"].as_array().is_none_or(|plugins| {
                        plugins.iter().any(|p| {
                            p["path"] != "builtin"
                                || !matches!(
                                    (p["name"].as_str(), p["source"].as_str()),
                                    (
                                        Some("cc-plugin-agents-md"),
                                        Some("cc-plugin-agents-md@builtin")
                                    ) | (
                                        Some("cc-plugin-privacy"),
                                        Some("cc-plugin-privacy@builtin")
                                    ) | (
                                        Some("cc-plugin-plugin-authoring"),
                                        Some("cc-plugin-plugin-authoring@builtin")
                                    )
                                )
                        })
                    })
                    || frame["agents"].as_array().is_none_or(|agents| {
                        agents.iter().any(|a| {
                            !matches!(
                                a.as_str(),
                                Some(
                                    "claude"
                                        | "Explore"
                                        | "general-purpose"
                                        | "Plan"
                                        | "statusline-setup"
                                )
                            )
                        })
                    })
                {
                    return Err("UNEXPECTED_TOOL_SURFACE");
                }
                self.initialized = true;
            }
            Some("stream_event") if self.initialized && frame["parent_tool_use_id"].is_null() => {
                let event = &frame["event"];
                match event["type"].as_str() {
                    Some("message_start") if !self.started => {
                        if event["message"]["model"]
                            .as_str()
                            .ok_or("MODEL_UNVERIFIED")?
                            != expected
                        {
                            return Err("MODEL_MISMATCH");
                        }
                        self.started = true;
                    }
                    Some("content_block_start")
                        if self.started && !self.ended && !self.end_turn =>
                    {
                        if !matches!(
                            event["content_block"]["type"].as_str(),
                            Some("text" | "thinking" | "redacted_thinking")
                        ) {
                            return Err("UNEXPECTED_TOOL_SURFACE");
                        }
                    }
                    Some("content_block_delta" | "content_block_stop")
                        if self.started && !self.ended && !self.end_turn => {}
                    Some("message_delta") if self.started && !self.ended && !self.end_turn => {
                        if event["delta"]["stop_reason"] != "end_turn" {
                            return Err("PROVIDER_INCOMPLETE");
                        }
                        self.end_turn = true;
                    }
                    Some("message_stop") if self.end_turn && !self.ended => self.ended = true,
                    _ => return Err("INVALID_PROVIDER_STREAM"),
                }
            }
            Some("assistant") if self.started && frame["parent_tool_use_id"].is_null() => {
                let parts = frame["message"]["content"]
                    .as_array()
                    .ok_or("INVALID_ASSISTANT")?;
                for part in parts {
                    if !matches!(
                        part["type"].as_str(),
                        Some("text" | "thinking" | "redacted_thinking")
                    ) {
                        return Err("UNEXPECTED_TOOL_SURFACE");
                    }
                }
            }
            Some("result") if self.ended => {
                if frame["is_error"] != false
                    || frame["subtype"] != "success"
                    || frame["stop_reason"] != "end_turn"
                    || frame["num_turns"] != 1
                    || frame["permission_denials"]
                        .as_array()
                        .is_none_or(|items| !items.is_empty())
                {
                    return Err("CLI_REPORTED_FAILURE");
                }
                let response = frame["result"].as_str().ok_or("MISSING_RESULT_TEXT")?;
                if response.is_empty() || response.len() > 16 * 1024 {
                    return Err("RESULT_TEXT_LIMIT");
                }
                self.result = Some(response.to_owned());
            }
            _ => return Err("UNKNOWN_FRAME"),
        }
        Ok(())
    }
}

pub(crate) fn validate(mut reader: impl BufRead, expected: &str) -> Result<String, &'static str> {
    if expected.is_empty() || expected.len() > 128 {
        return Err("INVALID_MODEL");
    }
    let mut state = Transcript::default();
    let mut total = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take((FRAME + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| "READ_FAILED")?;
        if count == 0 {
            break;
        }
        total += count;
        if count > FRAME || total > TOTAL {
            return Err("OUTPUT_LIMIT");
        }
        if line.last() != Some(&b'\n') {
            return Err("INCOMPLETE_FRAME");
        }
        let frame = serde_json::from_slice(&line).map_err(|_| "INVALID_JSON")?;
        state.message(&frame, expected)?;
    }
    state.result.ok_or("MISSING_TERMINAL")
}

pub fn run(args: &[String]) -> Result<(), u8> {
    let [expected] = args else {
        eprintln!("用法：relayctl adapter-verify-claude <expected-model> < transcript.ndjson");
        return Err(2);
    };
    match validate(std::io::stdin().lock(), expected) {
        Ok(_) => {
            println!(
                "{}",
                json!({"actual_model": expected, "provider_terminal": "end_turn"})
            );
            Ok(())
        }
        Err(code) => {
            eprintln!("{code}");
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_and_cli_must_both_finish() {
        let transcript = |model: &str, reason: &str, end: &str| {
            let init = r#"{"type":"system","subtype":"init","tools":[],"mcp_servers":[],"skills":[],"slash_commands":[],"permissionMode":"dontAsk","plugins":[],"agents":[]}"#;
            format!(
                "{init}\n{{\"type\":\"stream_event\",\"event\":{{\"type\":\"message_start\",\"message\":{{\"model\":\"{model}\"}}}}}}\n\
                 {{\"type\":\"stream_event\",\"event\":{{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\"}}}}}}\n\
                 {{\"type\":\"stream_event\",\"event\":{{\"type\":\"message_stop\"}}}}\n{end}"
            )
        };
        let success = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"stop_reason":"end_turn","permission_denials":[],"result":"test"}"#,
            "\n"
        );
        for (model, stop, result, expect) in [
            ("m", "end_turn", success, Ok("test".to_owned())),
            ("wrong", "end_turn", success, Err("MODEL_MISMATCH")),
            ("m", "max_tokens", success, Err("PROVIDER_INCOMPLETE")),
            ("m", "end_turn", "", Err("MISSING_TERMINAL")),
        ] {
            assert_eq!(
                validate(transcript(model, stop, result).as_bytes(), "m"),
                expect
            );
        }
        let tools = b"{\"type\":\"system\",\"subtype\":\"init\",\"tools\":[\"Bash\"]}\n";
        assert_eq!(
            validate(tools.as_slice(), "m"),
            Err("UNEXPECTED_TOOL_SURFACE")
        );
    }

    #[test]
    fn thinking_progress_frames_are_allowed_while_streaming() {
        let transcript = concat!(
            r#"{"type":"system","subtype":"init","tools":[],"mcp_servers":[],"skills":[],"slash_commands":[],"permissionMode":"dontAsk","plugins":[],"agents":[]}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"m"}}}"#,
            "\n",
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":1,"estimated_tokens_delta":1}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"thinking"}}}"#,
            "\n",
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":7,"estimated_tokens_delta":6}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#,
            "\n",
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"stop_reason":"end_turn","permission_denials":[],"result":"test"}"#,
            "\n"
        );
        assert_eq!(validate(transcript.as_bytes(), "m"), Ok("test".to_owned()));
    }
}
