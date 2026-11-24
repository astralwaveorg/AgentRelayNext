#!/usr/bin/env python3
"""Read-only MCP acceptance using the actual Codex App Server on macOS."""

import argparse
import json
import pathlib
import queue
import subprocess
import tempfile
import threading
import time

TOOLS = {
    "relay_plan", "relay_submit", "relay_status", "relay_list", "relay_artifact",
    "relay_cancel", "relay_retry", "relay_review",
}


def verify(root, cli, relay, socket):
    root = pathlib.Path(root)
    home, config = root / "home", root / "codex"
    home.mkdir(mode=0o700)
    config.mkdir(mode=0o700)
    (config / "config.toml").write_text(
        "[mcp_servers.relay]\nrequired = true\n"
        f"command = {json.dumps(str(relay))}\n"
        f'args = ["mcp", "--socket", {json.dumps(str(socket))}]\n'
    )
    sandbox = (
        "(version 1)(allow default)(deny network*)"
        f"(allow network-outbound (remote unix-socket (path {json.dumps(str(socket))})))"
    )
    env = {"PATH": "/usr/bin:/bin", "HOME": str(home), "CODEX_HOME": str(config)}
    with (root / "diagnostics").open("w+") as diagnostics, subprocess.Popen(
        ["/usr/bin/sandbox-exec", "-p", sandbox, cli, "app-server", "--listen", "stdio://"],
        cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=diagnostics, text=True,
    ) as server:
        messages = queue.Queue()

        def read():
            try:
                for line in server.stdout:
                    messages.put(json.loads(line))
            finally:
                messages.put(None)

        threading.Thread(target=read, daemon=True).start()

        def rpc(method, params=None, request_id=None):
            request = {"method": method}
            if params is not None:
                request["params"] = params
            if request_id is not None:
                request["id"] = request_id
            server.stdin.write(json.dumps(request) + "\n")
            server.stdin.flush()
            if request_id is None:
                return None
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                frame = messages.get(timeout=max(0.001, deadline - time.monotonic()))
                if frame is None:
                    raise RuntimeError("Codex App Server exited")
                if frame.get("id") == request_id:
                    if "error" in frame:
                        raise RuntimeError(f"{method}: {frame['error']}")
                    return frame["result"]
            raise TimeoutError(method)

        try:
            hello = rpc("initialize", {
                "clientInfo": {"name": "relay_acceptance", "title": "Relay Acceptance", "version": "1"},
                "capabilities": {"experimentalApi": True},
            }, 1)
            rpc("initialized")
            thread = rpc("thread/start", {
                "cwd": str(root), "ephemeral": True, "approvalPolicy": "never", "sandbox": "read-only",
            }, 2)["thread"]["id"]
            entries = rpc("mcpServerStatus/list", {"threadId": thread, "serverName": "relay"}, 3)
            entry, = entries["data"]
            if (entry["name"] != "relay" or entry["serverInfo"]["name"] != "relayctl-mcp"
                    or entry.get("toolsError") or set(entry["tools"]) != TOOLS):
                raise RuntimeError("incorrect MCP tool catalog")
            for name, tool in entry["tools"].items():
                schema = tool["inputSchema"]
                if not schema.get("properties") or not schema.get("required") or schema["additionalProperties"]:
                    raise RuntimeError(f"missing argument contract: {name}")

            def call(tool, arguments, request_id):
                return rpc("mcpServer/tool/call", {
                    "threadId": thread, "server": "relay", "tool": tool, "arguments": arguments,
                }, request_id)

            listed = call("relay_list", {"project_id": "relay_mcp_acceptance_nonexistent"}, 4)
            if listed.get("isError") is not False or listed["structuredContent"]["tasks"] != []:
                raise RuntimeError("list did not return a successful empty page")
            missing = call("relay_status", {"task_id": "task_relay_mcp_acceptance_nonexistent"}, 5)
            if missing.get("isError") is not True or missing["structuredContent"]["error"]["code"] != "NOT_FOUND":
                raise RuntimeError("missing task must report structured NOT_FOUND and isError=true")
            print(json.dumps({
                "codex": hello["userAgent"], "tools": sorted(entry["tools"]),
                "relay_list": "success", "relay_status": "NOT_FOUND", "model_turns_started": 0,
            }))
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("codex", type=pathlib.Path)
    parser.add_argument("relayctl", type=pathlib.Path)
    parser.add_argument("socket", type=pathlib.Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="relay-codex.", dir="/private/tmp") as root:
        verify(root, *(p.resolve(strict=True) for p in (args.codex, args.relayctl, args.socket)))


if __name__ == "__main__":
    main()
