#!/usr/bin/env python3
"""Build a sanitized evidence bundle from a live-project-acceptance sample.

Usage: evidence-bundle.py <sample-dir> [output.json]

The sample directory is the one printed by
`packaging/qa/live-project-acceptance.py prepare`. This script reads only
`evidence.json`, `sample.json`, and the sample's SQLite database, then writes a
reviewable JSON bundle. It never copies prompts, provider bodies, credentials,
account identifiers, or absolute user paths: binary paths collapse to basenames
and any unexpected free-text reason is redacted.
"""
import glob
import json
import os
import pathlib
import sqlite3
import sys
import time

ALLOWED_USAGE_KEYS = {
    "actual_model",
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "charge_attribution",
    "provider_reported_source",
    "balance_charge_usd",
    "allowance_usd",
}
FORBIDDEN = ("/Users/", "/home/", "sk-", "ghp_", "AKIA", "BEGIN PRIVATE KEY")


def fail(message: str) -> None:
    print(json.dumps({"status": "failed", "reason": message}))
    sys.exit(1)


def safe_reason(raw: str) -> str | None:
    if raw is None:
        return None
    if raw.startswith("round="):
        return raw
    try:
        value = json.loads(raw)
    except ValueError:
        return "<redacted>"
    if not isinstance(value, dict):
        return "<redacted>"
    kept = {k: v for k, v in value.items() if k in ALLOWED_USAGE_KEYS}
    return json.dumps(kept, sort_keys=True) if kept else "<redacted>"


def sqlite_rows(path: pathlib.Path, query: str):
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return list(con.execute(query))
    finally:
        con.close()


def main() -> None:
    if len(sys.argv) not in (2, 3):
        fail("usage: evidence-bundle.py <sample-dir> [output.json]")
    sample = pathlib.Path(sys.argv[1]).resolve()
    if not sample.is_dir():
        fail(f"not a directory: {sample}")
    evidence_path = sample / "evidence.json"
    sample_path = sample / "sample.json"
    if not evidence_path.is_file() or not sample_path.is_file():
        fail("sample is missing evidence.json or sample.json")
    evidence = json.loads(evidence_path.read_text())
    meta = json.loads(sample_path.read_text())

    databases = glob.glob(
        str(sample / "h" / ".agent-relay-next" / "projects" / "*" / "instance" / "relay.sqlite3")
    )
    if not databases:
        databases = glob.glob(str(sample / "**" / "instance" / "relay.sqlite3"), recursive=True)
    if not databases:
        fail("sample database not found")
    db = pathlib.Path(databases[0])
    task = evidence.get("task_id")
    if not task:
        fail("evidence has no task_id")

    task_rows = sqlite_rows(db, f"select state, reason_code from tasks where task_id='{task}'")
    if not task_rows:
        fail(f"task {task} not found in database")
    task_state, task_reason = task_rows[0]
    stages = [
        {"stage": stage, "round": round_no, "state": state}
        for stage, round_no, state in sqlite_rows(
            db,
            "select stage, round, state from stage_runs "
            f"where task_id='{task}' order by attempt_no, round, stage",
        )
    ]
    timeline = [
        {"seq": seq, "kind": kind, "state": state, "reason": safe_reason(reason)}
        for seq, kind, state, reason in sqlite_rows(
            db,
            "select seq, kind, state, reason_code from events "
            f"where task_id='{task}' order by seq",
        )
    ]

    usage = {}
    models = {}
    for entry in evidence.get("provider_usage", []):
        stage = entry.get("stage", "unknown")
        models[stage] = {
            "actual_model": entry.get("actual_model"),
            "charge_attribution": entry.get("charge_attribution"),
            "provider_reported_source": entry.get("provider_reported_source"),
        }
        usage[stage] = {
            key: entry.get(key)
            for key in (
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
                "allowance_usd",
                "balance_charge_usd",
            )
            if key in entry
        }

    cli = {
        name: os.path.basename(meta.get(name, ""))
        for name in ("claude", "opencode")
        if meta.get(name)
    }
    bundle = {
        "schema_version": 1,
        "kind": "synthetic_acceptance",
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "source": "live-project-acceptance prepare/run",
        "task_id": task,
        "status": evidence.get("status"),
        "task_state": task_state,
        "exit_reason": safe_reason(task_reason) if task_reason else None,
        "route_approval_sha256": evidence.get("approved_route_sha256"),
        "base_oid": evidence.get("base_oid"),
        "project_id": meta.get("project"),
        "clis": cli,
        "models": models,
        "usage": usage,
        "stages": stages,
        "digests": {
            "diff_sha256": evidence.get("diff_sha256"),
            "test_receipt_sha256": evidence.get("receipt_sha256"),
        },
        "timeline": timeline,
        "redactions": [
            "absolute binary and workspace paths (basenames only)",
            "credentials and account identifiers",
            "prompts, patches and raw provider bodies",
            "unexpected free-text failure reasons",
        ],
    }
    text = json.dumps(bundle, ensure_ascii=False, indent=2) + "\n"
    for marker in FORBIDDEN:
        if marker in text:
            fail(f"refusing to write: bundle contains {marker!r}")

    if len(sys.argv) == 3:
        out = pathlib.Path(sys.argv[2])
    else:
        out = sample / f"evidence-bundle-{task}.json"
    out.write_text(text)
    print(json.dumps({"status": "ok", "bundle": str(out), "task": task}))


if __name__ == "__main__":
    main()
