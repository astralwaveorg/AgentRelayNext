#!/usr/bin/env python3
"""Exercise daemon, persisted review stage and actual CLI against a loopback-only fixture."""
import argparse
from contextlib import ExitStack
import importlib.util
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid

SOURCE = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("loopback", Path(__file__).with_name("opencode-loopback.py"))
loop = importlib.util.module_from_spec(spec)
sys.dont_write_bytecode = True
spec.loader.exec_module(loop)
CANARY = "unshared fixture instruction 81fb9b6a"
BASE_TEXT = "pub fn count(v: &[u8]) -> usize {\n    v.len()\n}\n"


def case(root, relay, claude, model, expect, implement=None, opencode=False, tool=False, implement_cli=None,
         catalog=None, implement_opencode=False):
    root.mkdir(mode=0o700)
    repo, managed, instance, home = (root / n for n in ("repo", "managed", "inst", "home"))
    for folder in (repo, managed, instance, home):
        folder.mkdir(mode=0o700)
    env = {"PATH": "/usr/bin:/bin", "HOME": str(home),
           "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
           "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
    (repo / "src").mkdir()
    (repo / "tests").mkdir()
    (repo / "src/count.rs").write_text(BASE_TEXT)
    (repo / "tests/count.rs").write_text("// placeholder\n")
    (repo / "CLAUDE.md").write_text(CANARY)
    for cmd in (("init", "-q", "-b", "main"), ("add", "."), ("commit", "-qm", "fixture")):
        subprocess.run(["/usr/bin/git", "-C", repo, *cmd], check=True, env=env)
    rule = root / "verify.sh"
    if implement_cli:
        (root / "expected.rs").write_text(BASE_TEXT + "// relay implemented\n")
        rule.write_text('#!/bin/sh\nset -eu\nexec /usr/bin/cmp -s "${0%/*}/expected.rs" src/count.rs\n')
    else:
        rule.write_text("#!/bin/sh\nexit 0\n")
    rule.chmod(0o700)
    sock = instance / "relay.sock"

    with ExitStack() as stack:
        fixture = stack.enter_context(loop.Fixture(model, review=not implement or implement_cli is not None,
                              implement=implement if implement_cli is None else None,
                              stop_reason="tool_use" if tool else "end_turn", tool_reply=tool))
        implementation_model = "relay-opencode-review-model" if implement_opencode else loop.MODEL
        extra = stack.enter_context(loop.Fixture(implementation_model, implement=implement)) if implement_cli else None
        threads = [threading.Thread(target=f.serve_forever, daemon=True) for f in [fixture, extra] if f is not None]
        for thread in threads:
            thread.start()
        config = root / "conf.json"
        config.write_text(json.dumps({
            "repo_root": str(repo), "managed_root": str(managed),
            "adapter_catalog_dir": str(catalog or SOURCE / "crates/relay-daemon/mock/catalog"),
            ("opencode_review_fixture" if opencode else
             "claude_implement_fixture" if implement and not implement_opencode else "claude_review_fixture"):
                {"executable": str(claude), "port": fixture.server_address[1]},
            **({("opencode_implement_fixture" if implement_opencode else "claude_implement_fixture"):
                {"executable": str(implement_cli), "port": extra.server_address[1]}} if extra else {}),
            "worker_timeout_ms": 15000,
            "rules": [{"id": "mock_rule", "executable": str(rule), "argv": [], "timeout_ms": 2000}],
        }))
        with (root / "daemon.log").open("w") as diagnostics:
            daemon = subprocess.Popen([relay, "daemon", "--instance-dir", instance, "--config", config],
                                      env=env, stdout=diagnostics, stderr=diagnostics,
                                      start_new_session=True)
        try:
            def rpc(*argv, allow_failure=False):
                result = subprocess.run([relay, "--socket", sock, *argv], env=env, capture_output=True, timeout=4)
                if result.returncode and not allow_failure:
                    raise AssertionError(f"relay command failed: {argv[0]} {result.stderr[:500]!r} {result.stdout[:500]!r}")
                return json.loads(result.stdout)

            deadline = time.monotonic() + 3
            while not sock.is_socket() and daemon.poll() is None and time.monotonic() < deadline:
                time.sleep(0.025)
            if not sock.is_socket():
                raise RuntimeError((root / "daemon.log").read_text()[:1500])
            plan = rpc("plan", "--project-id", "proj_fixture", "--goal", "synthetic plan",
                       "--brief", "synthetic", "--profile-id", "prof_auto", "--scope", "src/count.rs",
                       "tests/count.rs", "--rules", "mock_rule")
            task = rpc("submit", "--plan-id", plan["plan_id"], "--request-id", str(uuid.uuid4()))["task"]["id"]
            deadline = time.monotonic() + 35
            while time.monotonic() < deadline:
                status = rpc("status", "--task-id", task)["task"]
                if status["state"] in ("awaiting_review", "failed", "interrupted", "blocked"):
                    break
                time.sleep(0.05)
            evidence = rpc("events", "--after", "0")
            if status["state"] != expect:
                raise AssertionError({"state": status["state"], "events": evidence, "requests": len(fixture.requests)})
            requested = 2 if implement == "rework" else 1
            if len(fixture.requests) != requested:
                raise AssertionError(f"expected {requested} requests, got {len(fixture.requests)}")
            for request, key, auth in fixture.requests:
                model_id = "relay-opencode-review-model" if opencode else loop.MODEL
                bad_fixture_header = not opencode and key != "relay-placeholder" and auth != "Bearer relay-placeholder"
                if bad_fixture_header or request.get("model") != model_id or request.get("tools"):
                    raise AssertionError("request used wrong profile or tools")
                if CANARY in json.dumps(request):
                    raise AssertionError("unapproved project instruction reached fixture")
            if expect == "failed":
                error = {"outside": "EDIT_PATH_OUTSIDE_APPROVAL", "stale": "EDIT_BASE_MISMATCH"}.get(implement, "UNEXPECTED_TOOL_RESPONSE" if tool else "MODEL_MISMATCH")
                if error not in json.dumps(evidence):
                    raise AssertionError(f"missing {error}")
            expected_stages = [("implement", "succeeded"), ("verify", "succeeded"), ("review", "succeeded")]
            if implement == "rework":
                expected_stages = expected_stages[:2] + [("review", "needs_changes")] + expected_stages
            elif implement in ("outside", "stale"):
                expected_stages = [("implement", "failed")]
            elif expect == "failed":
                expected_stages[-1] = ("review", "failed")
            db = sqlite3.connect(instance / "relay.sqlite3")
            stages = db.execute("SELECT stage, state FROM stage_runs ORDER BY round, CASE stage WHEN 'implement' THEN 1 WHEN 'verify' THEN 2 ELSE 3 END").fetchall()
            count = db.execute("SELECT count(*) FROM artifacts").fetchone()[0]
            db.close()
            assert stages == expected_stages, stages
            assert count == int(expect == "awaiting_review"), count
            if extra:
                assert len(extra.requests) == 1, len(extra.requests)
                impl_body, key, auth = extra.requests[0]
                assert impl_body["model"] == implementation_model and not impl_body.get("tools")
                if not implement_opencode:
                    assert key == "relay-placeholder" or auth == "Bearer relay-placeholder"
                assert CANARY not in json.dumps(impl_body)
            assert not list(managed.glob("review/*/*/opencode-private/profile")), "temporary profile survived"
            if implement:
                target = next(managed.glob("worktrees/*/attempt-*/src/count.rs"))
                original = (repo / "src/count.rs").read_text()
                assert "relay implemented" not in original
                expected_text = original + "// relay implemented\n" if implement in ("rework", "onepass") else original
                assert target.read_text() == expected_text
                assert (target.parents[1] / "CLAUDE.md").read_text() == CANARY
            print(json.dumps({"mode": "two-clis" if extra else "opencode-review" if opencode else implement or "review", "state": expect, "requests": requested, "stages": stages, "artifacts": count}), flush=True)
        finally:
            if daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    # Give the daemon its normal group-reclamation budget first.
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait(timeout=2)
            for f in [fixture, extra]:
                if f is not None:
                    f.shutdown()
            for thread in threads:
                thread.join(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("relay", type=Path)
    parser.add_argument("claude", type=Path)
    parser.add_argument("--opencode", action="store_true", help="use positional CLI path for OpenCode and run only those cases")
    parser.add_argument("--pair-opencode", type=Path, help="use positional Claude as implementer and this OpenCode as reviewer")
    parser.add_argument("--reverse-pair", type=Path, help="use this OpenCode as implementer and positional Claude as reviewer")
    parser.add_argument("--catalog", type=Path, help="use the packaged offline catalog")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires macOS")
    with tempfile.TemporaryDirectory(prefix="claude-review.", dir="/private/tmp") as tmp:
        if args.reverse_pair:
            case(Path(tmp) / "reverse", args.relay.resolve(strict=True), args.claude.resolve(strict=True),
                 loop.MODEL, "awaiting_review", implement="onepass",
                 implement_cli=args.reverse_pair.resolve(strict=True), implement_opencode=True,
                 catalog=args.catalog.resolve(strict=True) if args.catalog else None)
            return
        if args.pair_opencode:
            case(Path(tmp) / "pair", args.relay.resolve(strict=True), args.pair_opencode.resolve(strict=True),
                 "relay-opencode-review-model", "awaiting_review", implement="onepass", opencode=True,
                 implement_cli=args.claude.resolve(strict=True),
                 catalog=args.catalog.resolve(strict=True) if args.catalog else None)
            return
        if args.opencode:
            for name, model, state, tool in [
                ("oc-match", "relay-opencode-review-model", "awaiting_review", False),
                ("oc-wrong", "unexpected-model", "failed", False),
                ("oc-tool", "relay-opencode-review-model", "failed", True),
            ]:
                case(Path(tmp) / name, args.relay.resolve(strict=True), args.claude.resolve(strict=True), model, state, opencode=True, tool=tool)
            return
        for name, model, state, implement in [
            ("match", loop.MODEL, "awaiting_review", None),
            ("wrong", "fixture-model-mismatch", "failed", None),
            ("rework", loop.MODEL, "awaiting_review", "rework"),
            ("outside", loop.MODEL, "failed", "outside"),
            ("stale", loop.MODEL, "failed", "stale"),
        ]:
            case(Path(tmp) / name, args.relay.resolve(strict=True), args.claude.resolve(strict=True), model, state, implement)


if __name__ == "__main__":
    main()
