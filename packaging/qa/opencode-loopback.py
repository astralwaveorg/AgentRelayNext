#!/usr/bin/env python3
"""Actual OpenCode CLI, synthetic Anthropic streaming endpoint; macOS QA only."""
import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import sys
import subprocess
import tempfile
import threading

PROMPT = "Relay synthetic fixture. Produce the requested response. No project context."
MODEL = "relay-fixture-model"
PROVIDER = "minimax-coding-plan"


def emit(event, payload):
    return f"event: {event}\ndata: {json.dumps({'type': event, **payload})}\n\n"


class Fixture(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, model, stop_reason="end_turn", guarded=False, review=False, implement=None, isolated_review=False, tool_reply=False, validator=None):
        super().__init__(("127.0.0.1", 0), Handler)
        self.model = model
        self.stop_reason = stop_reason
        self.requests = []
        self.guarded = guarded
        self.review = review
        self.implement = implement
        self.isolated_review = isolated_review
        self.tool_reply = tool_reply
        self.validator = validator
        self.provider_calls = []
        self.rejected = []
        self.forwarded = 0


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def provider_error(self, status):
        payload = json.dumps({"type": "error", "error": {
            "type": "permission_error" if status == 403 else "invalid_request_error",
            "message": "Relay local fixture refused the request",
        }}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        if self.path.split("?", 1)[0] != "/anthropic/v1/messages":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length < 512 * 1024:
            self.send_error(413)
            return
        body = json.loads(self.rfile.read(length))
        self.server.requests.append(
            (body, self.headers.get("x-api-key"), self.headers.get("authorization"))
        )
        if self.server.guarded:
            if body.get("model") != MODEL or body.get("tools") or body.get("stream") is not True:
                self.server.rejected.append("REQUEST_NOT_ALLOWED")
                self.provider_error(403)
                return
            self.server.forwarded += 1
            if self.server.model != MODEL:
                self.server.rejected.append("MODEL_MISMATCH")
                self.provider_error(400)
                return
        if self.server.isolated_review:
            toolnames = [tool.get("name") for tool in body.get("tools", []) if isinstance(tool, dict)]
            main_prompt = [b.get("text") for m in body.get("messages", []) if m.get("role") == "user"
                           for b in m.get("content", []) if isinstance(b, dict) and b.get("type") == "text"]
            if (
                len(self.server.requests) != 1 or body.get("model") != MODEL or body.get("stream") is not True
                or (self.headers.get("x-api-key") != "relay-placeholder"
                    and self.headers.get("authorization") != "Bearer relay-placeholder")
                or toolnames not in ([], ["skill"]) or main_prompt != [PROMPT]
            ):
                self.server.rejected.append("REQUEST_NOT_ALLOWED")
                self.provider_error(403)
                return
            # The synthetic upstream receives a reconstructed request. Never forward
            # OpenCode's system prompts, tools, titles, headers, or environment.
            self.server.provider_calls.append({
                "model": MODEL, "stream": True, "max_tokens": 128,
                "messages": [{"role": "user", "content": "Return a review acknowledgement."}],
            })
        result = "relay fixture response"
        if self.server.review:
            try:
                blocks = [b for m in body["messages"] if m["role"] == "user" for b in m["content"]]
                review = next(json.loads(b["text"]) for b in blocks
                              if b.get("type") == "text" and b.get("text", "").startswith('{"candidate_diff_sha256":'))
                if set(review) != {"goal", "diff", "receipt", "candidate_diff_sha256", "test_receipt_sha256"}:
                    raise ValueError("unexpected review data")
                receipt = review["receipt"]
                if (set(receipt) != {"status", "rules"} or receipt["status"] != "pass" or
                        not receipt["rules"] or any(set(rule) != {"rule_id", "status"} or
                        rule["status"] != "pass" for rule in receipt["rules"])):
                    raise ValueError("unexpected test evidence")
                result = json.dumps({
                    "schema_version": 1, "verdict": "pass", "findings": [],
                    **{k: review[k] for k in ("candidate_diff_sha256", "test_receipt_sha256")},
                })
            except (KeyError, ValueError, StopIteration, TypeError):
                self.provider_error(400)
                return
        if self.server.implement:
            try:
                blocks = [b for m in body["messages"] if m["role"] == "user" for b in m["content"]]
                request = next(json.loads(b["text"]) for b in blocks
                               if b.get("type") == "text" and b.get("text", "").startswith('{"base_oid":'))
                file = next(f for f in request["files"] if f["path"] == "src/count.rs")
                line = "// relay implemented" if request["review_feedback"] or self.server.implement == "onepass" else "// TODO: fixture review requests rework"
                base = "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n"
                edits = [{**file, "text": f"{base}{line}\n"}]
                if self.server.implement == "outside":
                    edits.append({**file, "path": "CLAUDE.md", "text": "must never write"})
                if self.server.implement == "stale":
                    edits[0]["before_sha256"] = "0" * 64
                result = json.dumps({"schema_version": 1, "edits": edits})
            except (KeyError, ValueError, StopIteration, TypeError):
                self.provider_error(400)
                return
        block = {"type": "tool_use", "id": "fake-tool", "name": "skill", "input": {}} if self.server.tool_reply else {"type": "text", "text": ""}
        payload = "".join([
            emit("message_start", {"message": {
                "id": "msg_relay_fixture", "type": "message", "role": "assistant",
                "model": self.server.model, "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {"input_tokens": 8, "output_tokens": 1},
            }}),
            emit("content_block_start", {"index": 0, "content_block": block}),
            emit("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": result}}),
            emit("content_block_stop", {"index": 0}),
            emit("message_delta", {"delta": {"stop_reason": self.server.stop_reason},
                                   "usage": {"output_tokens": 4}}),
            emit("message_stop", {}),
        ]).encode()
        if self.server.isolated_review:
            check = subprocess.run([str(self.server.validator), "route-pilot", "verify-sse", MODEL],
                                   input=payload, capture_output=True, timeout=4)
            if check.returncode:
                self.server.rejected.append(check.stderr.decode().strip().removeprefix("route-pilot: "))
                self.provider_error(400)
                return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def execute(cli, root, model, stop_reason="end_turn", guarded=False, family="opencode", restricted=False, exact_port=False, isolated_review=False, tool_reply=False, validator=None):
    root.mkdir(mode=0o700)
    work = root / "work"
    work.mkdir(mode=0o700)
    blocks = root / "tmp"
    blocks.mkdir(mode=0o700)
    config = root / "opencode.json"
    with Fixture(model, stop_reason, guarded, isolated_review=isolated_review, tool_reply=tool_reply, validator=validator) as server:
        port = server.server_address[1]
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        config.write_text(json.dumps({
            "enabled_providers": [PROVIDER], "autoupdate": False, "share": "disabled",
            "instructions": [], "plugin": [], "mcp": {},
            **({"policies": [{"action": scope, "resource": "*", "effect": "deny"}
                           for scope in ("tool.use", "mcp.use", "skill.use", "plugin.use", "websearch.use")]}
                if restricted else {}),
            "permissions": [{"action": "*", "resource": "*", "effect": "deny"}],
            "agents": {"relay-fixture": {
                "description": "Relay synthetic review profile", "mode": "primary",
                "permissions": [{"action": "*", "resource": "*", "effect": "deny"}],
            }},
            "provider": {PROVIDER: {
                "npm": "@ai-sdk/anthropic", "name": "Relay localhost fixture",
                "options": {"baseURL": f"http://127.0.0.1:{port}/anthropic/v1", "apiKey": "relay-placeholder"},
                "models": {MODEL: {"name": MODEL, "limit": {"context": 32768, "output": 1024},
                                  **({"capabilities": {"tools": {"supported": False}}} if restricted else {})}},
            }},
        }))
        # Only local TCP is enabled. No project, user config, or credentials are mounted.
        # The private CLI server may use an OS-selected loopback port.
        profile = """
(version 1)
(allow default)
(deny file-write* process-exec network*)
(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/var/root"))
(allow process-exec file-read-data (literal (param "CLI")))
(allow file-write* (subpath (param "ROOT")) (literal "/dev/null"))
(allow network-bind network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"""
        if exact_port:
            profile = profile.replace('(remote ip "localhost:*")', f'(remote ip "localhost:{port}")')
        command = [
            "/usr/bin/sandbox-exec", "-D", f"CLI={cli}", "-D", f"ROOT={root}",
            "-p", profile, str(cli), "run", "--standalone", "--archived",
            "--format", "json", "--agent", "relay-fixture", "--model", f"{PROVIDER}/{MODEL}",
            *(["--title", "Relay explicit review"] if restricted else []), PROMPT,
        ]
        env = {
            "PATH": "/usr/bin:/bin", "HOME": str(root), "TMPDIR": str(blocks),
            "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
            "XDG_CACHE_HOME": str(root / "cache"), "XDG_STATE_HOME": str(root / "state"),
            "OPENCODE_CONFIG": str(config), "OPENCODE_DISABLE_MODELS_FETCH": "1",
            "OPENCODE_DISABLE_AUTOUPDATE": "1", "OPENCODE_DISABLE_DEFAULT_PLUGINS": "1",
            "OPENCODE_DISABLE_LSP_DOWNLOAD": "1", "OPENCODE_DISABLE_PROJECT_CONFIG": "1",
        }
        if family == "claude":
            command = command[:7] + [
                str(cli), "--bare", "--print", "--verbose", "--output-format", "stream-json",
                "--include-partial-messages", "--no-session-persistence", "--strict-mcp-config",
                "--mcp-config", '{"mcpServers":{}}', "--setting-sources", "", "--tools", "",
                "--disable-slash-commands", "--permission-mode", "dontAsk",
                "--model", MODEL, "--system-prompt", "Synthetic Relay fixture only",
            ]
            env.update({
                "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}/anthropic",
                "ANTHROPIC_API_KEY": "relay-placeholder",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
            })
        child = subprocess.Popen(command, cwd=work, env=env, stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True)
        try:
            out, err = child.communicate(input=PROMPT.encode() if family == "claude" else None, timeout=22)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate()
            raise RuntimeError(f"CLI timeout; {len(server.requests)} local requests, stdout={out[:250]!r}, stderr={err[:1500]!r}")
        finally:
            server.shutdown()
            thread.join(timeout=2)
        records = [json.loads(line) for line in out.splitlines() if line.strip()]
        return child.returncode, records, err.decode(errors="replace"), server.requests, server.rejected, server.provider_calls if isolated_review else server.forwarded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--frames-dir", type=Path, help="save synthetic Claude frames for offline replay")
    parser.add_argument("--validator", type=Path, help="validate the actual synthetic Claude NDJSON with relayctl")
    parser.add_argument("--claude", action="store_true", help="probe Claude Code with the same local stream")
    parser.add_argument("--guard-only", action="store_true", help="test the restricted-request guard only")
    parser.add_argument("--review-guard", action="store_true", help="qualify guarded review using only synthetic upstream")
    args = parser.parse_args()
    cli = args.binary.resolve(strict=True)
    if sys.platform != "darwin":
        parser.error("requires the macOS QA sandbox")
    with tempfile.TemporaryDirectory(prefix="relay-route.", dir="/private/tmp") as directory:
        root = Path(directory)
        if args.claude:
            for name, response in [("matched", MODEL), ("model_mismatch", "fixture-wrong-upstream-model")]:
                rc, records, err, requests, rejected, forwarded = execute(
                    cli, root / name, response, family="claude"
                )
                if not requests:
                    raise RuntimeError(f"{name}: no request to loopback; exit {rc}, stderr {err[:800]!r}")
                serialized = json.dumps(records)
                frames = "".join(json.dumps(record) + "\n" for record in records)
                if args.frames_dir:
                    with (args.frames_dir / (name + ".ndjson")).open("x") as f:
                        f.write(frames)
                if args.validator:
                    proc = subprocess.run([str(args.validator.resolve()), "adapter-verify-claude", MODEL],
                                          input=frames, text=True, capture_output=True, timeout=4)
                    print(json.dumps({"scenario": name, "relay_exit": proc.returncode,
                                      "relay_summary": proc.stdout.strip() or proc.stderr.strip()}))
                    if (proc.returncode == 0) != (response == MODEL):
                        raise RuntimeError("Relay validator accepted the wrong outcome")
                    if response != MODEL and "MODEL_MISMATCH" not in proc.stderr:
                        raise RuntimeError("Relay validator did not explicitly reject an identity mismatch")
                print(json.dumps({"scenario": name, "exit": rc, "requests": len(requests),
                    "request_models": sorted({body.get("model", "") for body, _, _ in requests}),
                    "tools": sorted({tool.get("name", "") for body, _, _ in requests for tool in body.get("tools", [])}),
                    "response_model_in_cli": response in serialized,
                    "result_subtypes": [e.get("subtype") for e in records if e.get("type") == "result"]}), flush=True)
            return
        if args.review_guard:
            if not args.validator:
                parser.error("--review-guard requires --validator <relayctl>")
            for name, model, stop, tool, expected in [
                ("normal", MODEL, "end_turn", False, None),
                ("wrong_model", "unexpected-model", "end_turn", False, "MODEL_MISMATCH"),
                ("tool_response", MODEL, "tool_use", True, "UNEXPECTED_TOOL_RESPONSE"),
                ("truncated", MODEL, "max_tokens", False, "PROVIDER_INCOMPLETE"),
            ]:
                rc, frames, _, calls, refused, forwarded = execute(
                    cli, root / name, model, stop_reason=stop, restricted=True,
                    isolated_review=True, tool_reply=tool, validator=args.validator.resolve(strict=True),
                )
                finishes = [f.get("part", {}).get("reason") for f in frames if f.get("type") == "step_finish"]
                if len(forwarded) != 1 or any("tools" in p or "system" in p for p in forwarded):
                    raise AssertionError("upstream escaped its one tool-free request")
                if expected is None:
                    if rc or len(calls) != 1 or refused or finishes != ["stop"]:
                        raise AssertionError({"mode": name, "rc": rc, "calls": len(calls), "refused": refused, "finishes": finishes})
                elif rc == 0 or finishes or not refused or refused[0] != expected:
                    raise AssertionError({"mode": name, "rc": rc, "calls": len(calls), "refused": refused, "finishes": finishes})
                print(json.dumps({"mode": name, "cli_exit": rc, "local_requests": len(calls), "simulated_upstream": len(forwarded),
                                  "provider_tools": 0, "refused": refused, "external_requests": 0}), flush=True)
            return
        if args.guard_only:
            failures = []
            for name, response in [("guarded_match", MODEL), ("guarded_mismatch", "different-model")]:
                rc, records, err, requests, rejected, forwarded = execute(cli, root / name, response, guarded=True)
                finishes = [e.get("part", {}).get("reason") for e in records if e.get("type") == "step_finish"]
                report = {"scenario": name, "exit": rc, "requests": len(requests), "forwarded": forwarded,
                          "blocked": sorted(rejected), "finishes": finishes}
                print(json.dumps(report), flush=True)
                if "REQUEST_NOT_ALLOWED" not in rejected or forwarded < 1:
                    raise RuntimeError(f"unexpected provider guard behavior: {report}; {err[:500]}")
                if forwarded > 1:
                    failures.append("REPEATED_REQUEST_AFTER_IDENTITY_REFUSAL")
                if response == MODEL and (rc or finishes != ["stop"]):
                    failures.append("DENYING_BUILTIN_REQUEST_BREAKS_NORMAL_RUN")
                if response != MODEL and (rc == 0 or finishes or "MODEL_MISMATCH" not in rejected):
                    failures.append("UPSTREAM_MODEL_MISMATCH_NOT_REJECTED")
            print(json.dumps({"qualification": "blocked" if failures else "offline-pass", "blockers": failures}))
            if failures:
                raise SystemExit(2)
            return
        failures = []
        for name, response_model, stop_reason in [
            ("matched", MODEL, "end_turn"),
            ("model_mismatch", "fixture-wrong-upstream-model", "end_turn"),
            ("max_tokens", MODEL, "max_tokens"),
        ]:
            rc, records, err, requests, _, _ = execute(cli, root / name, response_model, stop_reason)
            if not requests:
                raise RuntimeError(f"{name}: no request reached loopback; exit {rc}, stderr {err[:1000]!r}")
            if not all(
                body.get("model") == MODEL and body.get("stream") is True
                and (key == "relay-placeholder" or auth == "Bearer relay-placeholder")
                for body, key, auth in requests
            ):
                raise RuntimeError(f"{name}: unexpected request model, stream, or credential reference")
            serialized = json.dumps(records)
            tools = sorted({tool["name"] for body, _, _ in requests for tool in body.get("tools", [])})
            aliases = [key for key in ("model", "modelID", "providerID") if f'"{key}"' in serialized]
            reasons = [
                event.get("part", {}).get("reason") for event in records
                if event.get("type") == "step_finish"
            ]
            print(json.dumps({
                "scenario": name, "exit": rc, "loopback_requests": len(requests),
                "tools_advertised": tools, "event_kinds": [event.get("type") for event in records],
                "response_model_in_cli": response_model in serialized, "identity_keys": aliases,
                "finish_reasons": reasons,
            }, ensure_ascii=False))
            if name == "matched":
                if rc or reasons != ["stop"] or "relay fixture response" not in serialized:
                    raise RuntimeError("matched stream did not reach a normal terminal")
                if tools:
                    failures.append("UNEXPECTED_PROVIDER_TOOLS")
            if name == "model_mismatch" and not (rc != 0 or "error" in [e.get("type") for e in records]):
                failures.append("UPSTREAM_MODEL_MISMATCH_NOT_REJECTED")
            if name == "max_tokens" and rc == 0 and reasons == ["stop"]:
                failures.append("TRUNCATION_REPORTED_AS_COMPLETE")
        print(json.dumps({"qualification": "blocked" if failures else "offline-pass",
                          "blockers": failures}, ensure_ascii=False))
        if failures:
            raise SystemExit(2)


if __name__ == "__main__":
    main()
