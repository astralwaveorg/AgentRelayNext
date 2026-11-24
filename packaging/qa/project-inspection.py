import json, os, pathlib, selectors, signal, socket, subprocess, sys, tempfile
bin = pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="rp-",dir="/private/tmp") as tmp:
    root=pathlib.Path(tmp)
    home=root/"h"; home.mkdir()
    env=dict(os.environ,HOME=str(home))
    repo=root/"r"; repo.mkdir()
    def git(*args):
        return subprocess.check_output(["/usr/bin/git","-C",repo,*args],env=env)
    git("init","-q"); (repo/"status.txt").write_text("base\n")
    git("add","status.txt"); git("-c","user.name=QA","-c","user.email=qa@local.invalid","commit","-qm","base")
    head=git("rev-parse","HEAD")
    initial_index=(repo/".git/index").read_bytes()
    def cli(*args, data=None):
        return json.loads(subprocess.check_output([bin,"project-setup",*args],input=data,env=env))
    assert cli("list")["projects"] == []
    draft=json.dumps(dict(name="QA",repo_root=str(repo),write_scope=["status.txt"],
        rule=dict(id="verify",executable="/usr/bin/touch",argv=[str(root/"must-not-run")],timeout_ms=1000))).encode()
    a=cli("preview",data=draft)
    assert not (home/".agent-relay-next").exists()
    path=pathlib.Path(cli("save","--approve",a["approval"],data=draft)["path"])
    row,=cli("list")["projects"]; id=row["id"]; assert row["name"]=="QA"
    def spawn(): return subprocess.Popen([bin,"project-setup","serve","--project-id",id],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
    def ready_line(proc):
        with selectors.DefaultSelector() as ready:
            ready.register(proc.stdout, selectors.EVENT_READ)
            assert ready.select(timeout=15), "project service did not report readiness"
        return proc.stdout.readline()

    def rpc(endpoint, method, params):
        with socket.socket(socket.AF_UNIX) as c:
            c.settimeout(3)
            c.connect(str(endpoint))
            with c.makefile("rwb") as f:
                for i, m, p in [(1, "hello", {"version": 1}), (2, method, params)]:
                    f.write(json.dumps(dict(jsonrpc="2.0", id=i, method=m, params=p)).encode() + b"\n")
                    f.flush()
                    reply = json.loads(f.readline())
                    assert reply["id"] == i
                return reply["result"]

    # Activate the real route configuration with fake accounts but never submit
    # a task. This validates the installer's actual preview/approval/profile path
    # without spending or sending an HTTP request.
    claude = root / "fixture-claude"
    go = root / "fixture-go"
    for tool in [claude, go]:
        tool.write_text(f"#!/bin/sh\n# {tool.name}\nexit 97\n")
        tool.chmod(0o700)
    args = id, str(claude), str(go)
    preview = cli("route-preview", *args)
    assert preview["details"]["route"]["max_rounds"] == 1
    assert not (path.parent / "instance").exists()
    altered = subprocess.run(
        [bin, "project-setup", "serve-live", *args, "--approve", "stale",
         "--allow-plan-credits", "--extra-usage-off"], input=b"", capture_output=True, env=env)
    assert altered.returncode != 0 and not (path.parent / "instance").exists()
    no_billing_ack = subprocess.run(
        [bin, "project-setup", "serve-live", *args, "--approve", preview["approval"]],
        input=b"", capture_output=True, env=env)
    assert no_billing_ack.returncode != 0 and not (path.parent / "instance").exists()
    with subprocess.Popen(
        [bin, "project-setup", "serve-live", *args, "--approve", preview["approval"],
         "--allow-plan-credits", "--extra-usage-off"],
        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ) as live:
        endpoint = None
        try:
            live.stdin.write(b"sk-cp-pretend-claude-key\npretend-opencode-key\n")
            live.stdin.close()
            endpoint = pathlib.Path(json.loads(ready_line(live))["socket"])
            proj = rpc(endpoint, "ui.project", {})
            assert proj["dispatch_enabled"] is True and proj["external_route"] == preview["details"]["route"]
            plan = dict(project_id=id, profile_id=proj["profile_id"], goal="preview only",
                        brief="not submitted", write_scope=["status.txt"], verification_rule_ids=["verify"])
            planned = rpc(endpoint, "relay_plan", plan)
            assert planned["resolved"]["external_route"] == proj["external_route"]
            assert planned["resolved"]["base_oid"] == head.decode().strip()
            assert rpc(endpoint, "ui.overview", {})["tasks"] == []
        finally:
            if live.poll() is None:
                live.send_signal(signal.SIGINT)
            try:
                assert live.wait(timeout=8) == 0, live.stderr.read()
                assert endpoint is not None and not endpoint.exists()
                assert not (endpoint.parent / "relay.sock.own").exists()
            finally:
                if live.poll() is None:
                    live.kill()
                    live.wait(timeout=3)

    for _ in range(2):
        with spawn() as proc:
            ready = json.loads(ready_line(proc))
            endpoint = pathlib.Path(ready["socket"])
            assert ready["project_id"] == id and endpoint.parent.parent == path.parent
            assert rpc(endpoint, "ui.project", {})["dispatch_enabled"] is False
            assert rpc(endpoint, "ui.overview", {})["tasks"] == []
            for method in ["relay_plan", "relay_submit", "relay_retry", "relay_review", "relay_cancel"]:
                assert rpc(endpoint, method, {})["error"]["code"] == "SERVICE_UNAVAILABLE"
            assert list((path.parent / "managed").iterdir()) == []
            proc.stdin.close()
            assert proc.wait(timeout=5) == 0 and not endpoint.exists(), proc.stderr.read()
    assert not (root/"must-not-run").exists()
    assert (repo/"status.txt").read_bytes()==b"base\n" and (repo/".git/index").read_bytes()==initial_index
    assert git("rev-parse","HEAD")==head and not git("status","--porcelain")
    moved=root/"original"; repo.rename(moved); repo.mkdir(); git("init","-q")
    with spawn() as proc:
        assert not proc.stdout.readline() and proc.wait(timeout=5)!=0
    print("project registration → list → SIGINT live shutdown → 2 isolated starts → mutation rejection → EOF shutdown → replacement rejected: OK")
