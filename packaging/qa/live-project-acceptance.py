#!/usr/bin/env python3
"""One-shot acceptance of the approved real route in an isolated synthetic project."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import stat
import subprocess
import sys
import tempfile
import time
import uuid

START = b"status=waiting\n"
TARGET = b"status=ready\n"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def capture(args, env, data=None):
    source = {"stdin": subprocess.DEVNULL} if data is None else {"input": data}
    return subprocess.check_output(args, env=env, timeout=20, **source)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    commands = p.add_subparsers(dest="command", required=True)
    prep = commands.add_parser("prepare", help="local preview only; no accounts or network")
    for key in ("relayctl", "claude", "opencode"):
        prep.add_argument(key, type=Path)
    live = commands.add_parser("run", help="send one synthetic implementation and at most one review")
    live.add_argument("directory", type=Path)
    live.add_argument("--approve", required=True)
    live.add_argument("--allow-plan-credits", required=True, action="store_true")
    live.add_argument("--extra-usage-off", required=True, action="store_true")
    args = p.parse_args()
    os.umask(0o077)
    root = (Path(tempfile.mkdtemp(prefix="ar-", dir="/private/tmp"))
            if args.command == "prepare" else args.directory.resolve(strict=True))
    info = root.stat()
    if not root.is_dir() or (info.st_uid, stat.S_IMODE(info.st_mode)) != (os.getuid(), 0o700):
        raise RuntimeError("acceptance directory must be owned by this user and 0700")
    home, repo = root / "h", root / "repo"
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin", "GIT_CONFIG_NOSYSTEM": "1"}
    env.update({f"GIT_{role}_{key}": val for role in ("AUTHOR", "COMMITTER")
                for key, val in (("NAME", "Relay QA"), ("EMAIL", "qa@example.invalid"))})
    # Optional route overrides: the sandboxed child starts from a cleared
    # environment, so pass the pinned endpoint and review model through.
    if endpoint := os.environ.get("RELAY_MINIMAX_ENDPOINT"):
        env["RELAY_MINIMAX_ENDPOINT"] = endpoint
    if review_model := os.environ.get("RELAY_GO_MODEL"):
        env["RELAY_GO_MODEL"] = review_model
    git = lambda directory, *a: capture(["/usr/bin/git", "-C", directory, *a], env)
    state = root / "sample.json"
    result = root / "evidence.json"
    rule = root / "expected.txt"
    project_file = repo / "status.txt"

    if args.command == "prepare":
        home.mkdir(mode=0o700)
        repo.mkdir(mode=0o700)
        project_file.write_bytes(START)
        rule.write_bytes(TARGET)
        git(repo, "init", "-q", "-b", "main")
        git(repo, "add", "--", "status.txt")
        git(repo, "commit", "-qm", "Synthetic sample")
        relay, claude, opencode = (str(a.resolve(strict=True)) for a in
                                  (args.relayctl, args.claude, args.opencode))
        draft = json.dumps({"name": "Relay synthetic acceptance", "repo_root": str(repo),
                            "write_scope": ["status.txt"],
                            "rule": {"id": "exact_bytes", "executable": "/usr/bin/cmp",
                                     "argv": ["-s", str(rule), "status.txt"], "timeout_ms": 2000}}).encode()
        setup = lambda *a, data=None: json.loads(capture([relay, "project-setup", *a], env, data))
        preview = setup("preview", data=draft)
        setup("save", "--approve", preview["approval"], data=draft)
        project = preview["registration"]["project_binding"]["project_id"]
        route = setup("route-preview", project, claude, opencode)
        state.write_text(json.dumps({"relay": relay, "claude": claude, "opencode": opencode,
                                     "project": project, "base": git(repo, "rev-parse", "HEAD").decode().strip(),
                                     "approval": route["approval"]}))
        print(json.dumps({"directory": str(root), **route}, ensure_ascii=False, indent=2))
        return

    if sys.stdin.isatty():
        raise RuntimeError("pipe the two dedicated keys through stdin; do not paste them into the terminal")
    sample = json.loads(state.read_text())
    relay, claude, opencode, project = (sample[k] for k in ("relay", "claude", "opencode", "project"))
    if sample["approval"] != args.approve:
        raise RuntimeError("approval does not match the prepared sample")
    if (base := git(repo, "rev-parse", "HEAD").decode().strip()) != sample["base"] or (
        project_file.read_bytes() != START or rule.read_bytes() != TARGET or git(repo, "status", "--porcelain")
    ):
        raise RuntimeError("synthetic repository or rule changed; prepare a fresh sample")
    # An uncertain outcome can still incur usage. Never resend this sample.
    with (root / "run.started").open("x") as marker:
        marker.write("single attempt; inspect evidence and provider usage before another sample\n")

    evidence = {"base_oid": base, "approved_route_sha256": args.approve,
                "status": "incomplete"}
    proc = None
    try:
        with (root / "service.log").open("xb") as log:
            proc = subprocess.Popen([relay, "project-setup", "serve-live", project, claude, opencode,
                                     "--approve", args.approve, "--allow-plan-credits", "--extra-usage-off"],
                                    env=env, stdin=sys.stdin, stdout=subprocess.PIPE, stderr=log)
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=25):
                raise RuntimeError("project service did not report readiness; see service.log")
        socket = json.loads(proc.stdout.readline())["socket"]
        rpc = lambda *a: json.loads(capture([relay, "--socket", socket, *a], env))
        cfg = rpc("project")
        plan = rpc("plan", "--project-id", project, "--profile-id", cfg["profile_id"],
                   "--base-oid", sample["base"], "--scope", "status.txt", "--rules", "exact_bytes",
                   "--goal", "Change status=waiting to status=ready in status.txt, preserving the final newline.",
                   "--brief", "Synthetic QA only; edit this one file. The exact-byte local test checks the change.")
        if plan["resolved"]["external_route"] != cfg["external_route"]:
            raise RuntimeError("plan route differs from approved service")
        task = rpc("submit", "--plan-id", plan["plan_id"], "--request-id", str(uuid.uuid4()))["task"]["id"]
        evidence["task_id"] = task
        deadline = time.monotonic() + 150
        while True:
            latest = rpc("status", "--task-id", task)["task"]
            if (latest["state"] not in ("queued", "preparing", "launching", "running", "verifying")
                    or time.monotonic() >= deadline):
                break
            time.sleep(0.5)
        events = [e for e in rpc("events", "--limit", "100")["events"] if e["task_id"] == task]
        evidence["task_state"] = latest["state"]
        evidence["failures"] = [e["reason_code"] for e in events if e["kind"] == "worker_failed"]
        evidence["provider_usage"] = usages = [
            {"stage": e["kind"], **json.loads(e["reason_code"])}
            for e in events if e["kind"] in ("route_implement_usage", "route_review_usage")]
        if latest["state"] != "awaiting_review" or latest["active_attempt"] != 1:
            raise RuntimeError(f"task ended in {latest['state']}; inspect evidence.json")
        if [u["stage"] for u in usages] != ["route_implement_usage", "route_review_usage"] or usages[1]["balance_charge_usd"] != 0:
            raise RuntimeError("missing stage or Go did not report zero balance charge")

        def artifact(kind):
            page = rpc("artifact", "--task-id", task, "--attempt-no", "1", "--kind", kind)
            if page["next_offset"] is not None:
                raise RuntimeError("unexpected paged synthetic artifact")
            return page["content"].encode()

        patch, receipt = artifact("diff"), artifact("test_receipt")
        digest, test_digest = sha(patch), sha(receipt)
        review = json.loads(artifact("handoff"))["workflow"]["review"]
        if (json.loads(receipt)["status"] != "pass" or review["verdict"] != "pass"
                or review["candidate_diff_sha256"] != digest
                or review["test_receipt_sha256"] != test_digest):
            raise RuntimeError("independent verification or review did not pass for these bytes")
        replica = root / "apply"
        replica.mkdir(mode=0o700)
        (replica / "status.txt").write_bytes(START)
        capture(["/usr/bin/git", "-C", replica, "apply", "--", "-"], env, patch)
        if (replica / "status.txt").read_bytes() != TARGET or list(replica.iterdir()) != [replica / "status.txt"]:
            raise RuntimeError("applied patch differs from the exact synthetic change")
        if (project_file.read_bytes() != START or git(repo, "status", "--porcelain")
                or git(repo, "rev-parse", "HEAD").decode().strip() != sample["base"]):
            raise RuntimeError("original checkout changed")
        completed = rpc("review", "--task-id", task, "--attempt-no", "1",
                        "--expected-revision", str(latest["revision"]), "--request-id", str(uuid.uuid4()),
                        "--verdict", "accept", "--diff-sha256", digest, "--test-sha256", test_digest)
        if completed["task"]["state"] != "completed":
            raise RuntimeError("synthetic acceptance did not reach completed")
        evidence.update(status="completed", task_state="completed", diff_sha256=digest,
                        receipt_sha256=test_digest)
    finally:
        try:
            if proc is not None and proc.poll() is None:
                proc.terminate()
                proc.wait(timeout=12)
        finally:
            result.write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n")
            print(f"Evidence: {result}", file=sys.stderr)
    print(json.dumps(evidence, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, subprocess.SubprocessError, ValueError, KeyError, RuntimeError) as error:
        print(f"Acceptance stopped: {error}", file=sys.stderr)
        sys.exit(1)
