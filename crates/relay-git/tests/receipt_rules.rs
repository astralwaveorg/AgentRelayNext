//! WP2-C2 测试收据契约测试：绑定 attempt/规则 ID/被测补丁 SHA-256/实际
//! 运行结果；源码被改写、差异漂移与规则失败不出具通过收据；摘要上限。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_git::{collect_diff, create_worktree, run_and_build_receipt, VerifyError, WriteScope};

fn git_out(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-git-receipt-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    fs::write(root.join("b.txt"), "two\n").unwrap();
    git_out(&root, &["add", "a.txt", "b.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    root
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn setup_worktree(dir: &Path, task: &str) -> (PathBuf, String, PathBuf) {
    let root = init_repo(dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), task, 1, &head).unwrap();
    (root, head, report.path)
}

#[test]
fn compliant_changes_with_passing_rule_get_pass_receipt() {
    let dir = temp_dir("c2-pass");
    let (_root, head, wt) = setup_worktree(&dir, "task_c2a");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    let script = write_script(&dir, "pass.sh", "exit 0");
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_pass".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_pass".to_owned()],
        &wt,
        &head,
        "task_c2a-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();

    assert_eq!(receipt.status, "pass");
    assert!(receipt.reason.is_none());
    assert_eq!(receipt.attempt, "task_c2a-1");
    assert_eq!(receipt.base_oid, head);
    // 绑定被测补丁摘要。
    let expected_sha = {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(tested.patch.as_bytes()).into();
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(receipt.diff_sha256.as_deref(), Some(expected_sha.as_str()));
    assert_eq!(receipt.rules.len(), 1);
    assert_eq!(receipt.rules[0].rule_id, "rule_pass");
    assert_eq!(receipt.rules[0].status, "pass");
    assert_eq!(receipt.rules[0].started_at, "2026-11-22T02:00:00Z");
    // JSON 含关键绑定字段。
    assert!(receipt.json.contains("\"schema_version\":1"));
    assert!(receipt
        .json
        .contains(&format!("\"diff_sha256\":\"{expected_sha}\"")));
    assert!(receipt.json.contains("\"rule_id\":\"rule_pass\""));

    remove_dir(&dir);
}

#[test]
fn patch_digest_mismatch_yields_failed_receipt() {
    let dir = temp_dir("c2-digest");
    let (_root, head, wt) = setup_worktree(&dir, "task_c2b");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    // 被测补丁声称的修改与实际 worktree 状态不一致（摘要不匹配）。
    let tested = collect_diff(&wt, &head, &scope).unwrap();
    let mismatched = relay_git::CollectedDiff {
        patch: tested.patch.replace("one-modified\n", "never-happened\n"),
        changes: tested.changes.clone(),
    };

    // marker 规则放在列表末尾：摘要不匹配时任何规则都不得启动。
    let marker = dir.join("marker");
    let script = write_script(&dir, "marker.sh", &format!("touch {}", marker.display()));
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_any".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_any".to_owned()],
        &wt,
        &head,
        "task_c2b-1",
        &mismatched,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();

    assert_eq!(receipt.status, "failed", "摘要不匹配不得出具通过收据");
    // 零调用：marker 未被创建。
    assert!(!marker.exists(), "摘要不匹配时任何规则都不得启动");
    assert!(receipt.rules.is_empty(), "零调用：收据不得有规则条目");
    assert!(
        receipt
            .reason
            .as_deref()
            .unwrap()
            .contains("与被测补丁不一致"),
        "{receipt:?}"
    );
    // diff_sha256 绑定实际验证到的差异，而非被声称的补丁。
    let actual_sha = {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(tested.patch.as_bytes()).into();
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(receipt.diff_sha256.as_deref(), Some(actual_sha.as_str()));

    remove_dir(&dir);
}

#[test]
fn source_modified_during_verification_fails_receipt() {
    let dir = temp_dir("c2-modified");
    let (_root, head, wt) = setup_worktree(&dir, "task_c2c");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    // 替身规则在验证期间改写已跟踪源文件（a.txt 属写入范围）。
    let script = write_script(
        &dir,
        "tamper.sh",
        &format!("echo tampered >> {}/a.txt", wt.display()),
    );
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_tamper".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_tamper".to_owned()],
        &wt,
        &head,
        "task_c2c-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();

    assert_eq!(receipt.status, "failed", "源码被改写不得出具通过收据");
    assert!(
        receipt
            .reason
            .as_deref()
            .unwrap()
            .contains("验证改动了工作副本"),
        "{receipt:?}"
    );
    // 收据记录验证后的差异摘要，供审计。
    let post_sha = {
        use sha2::{Digest, Sha256};
        let post = collect_diff(&wt, &head, &scope).unwrap();
        let digest: [u8; 32] = Sha256::digest(post.patch.as_bytes()).into();
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert!(receipt
        .json
        .contains(&format!("\"post_diff_sha256\":\"{post_sha}\"")));
    assert!(receipt.json.contains("\"source_modified\":true"));

    remove_dir(&dir);
}

#[test]
fn failing_rule_and_size_cap_are_enforced() {
    let dir = temp_dir("c2-fail-cap");
    let (_root, head, wt) = setup_worktree(&dir, "task_c2d");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    // 规则失败 → failed 收据。
    let fail_script = write_script(&dir, "fail.sh", "exit 3");
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_fail".into(),
        executable: fail_script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_fail".to_owned()],
        &wt,
        &head,
        "task_c2d-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();
    assert_eq!(receipt.status, "failed");
    assert_eq!(receipt.rules[0].status, "fail");
    assert_eq!(receipt.rules[0].exit_code, Some(3));

    // 摘要上限：替身输出 10 KiB，收据内摘要必须 ≤ 2 KiB（含截断标记）。
    let big_script = write_script(&dir, "big.sh", "head -c 10240 /dev/zero | tr '\\0' 'x'");
    let big_config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_big".into(),
        executable: big_script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = relay_git::run_rule(&big_config, "rule_big", &wt).unwrap();
    assert!(
        outcome.output_excerpt.len() <= relay_git::MAX_OUTPUT_EXCERPT_BYTES + 20,
        "摘要应截断到 2 KiB 级: {}",
        outcome.output_excerpt.len()
    );

    // 收据 64 KiB 上限：34 条规则 × ~2 KiB 摘要 > 64 KiB → 报错不截断。
    let mut rules = Vec::new();
    let mut ids = Vec::new();
    for i in 0..34 {
        let script = write_script(
            &dir,
            &format!("big{i}.sh"),
            "head -c 10240 /dev/zero | tr '\\0' 'y'",
        );
        rules.push(relay_git::VerifyRule {
            id: format!("rule_big{i}"),
            executable: script.to_str().unwrap().into(),
            argv: vec![],
            timeout_ms: 5_000,
        });
        ids.push(format!("rule_big{i}"));
    }
    let big_config = relay_git::VerifyConfig::new(rules).unwrap();
    let err = run_and_build_receipt(
        &big_config,
        &ids,
        &wt,
        &head,
        "task_c2d-2",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap_err();
    assert!(
        matches!(err, relay_git::ReceiptError::ReceiptTooLarge { size } if size > 64 * 1024),
        "{err:?}"
    );

    // 空规则集不允许出具通过收据。
    let err = run_and_build_receipt(
        &relay_git::VerifyConfig::new(vec![]).unwrap(),
        &[],
        &wt,
        &head,
        "task_c2d-3",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap_err();
    assert!(matches!(err, relay_git::ReceiptError::NoRules), "{err:?}");

    remove_dir(&dir);
}

#[test]
fn missing_tool_produces_failed_receipt_with_not_run_entry() {
    let dir = temp_dir("c2-fix-tool");
    let (_root, head, wt) = setup_worktree(&dir, "task_f4");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_missing".into(),
        executable: "/nonexistent/tool-xyz".into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_missing".to_owned()],
        &wt,
        &head,
        "task_f4-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();

    assert_eq!(receipt.status, "failed", "工具缺失 → failed 收据");
    assert_eq!(receipt.rules.len(), 1);
    assert_eq!(receipt.rules[0].status, "not_run");
    assert!(
        receipt.rules[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("工具未找到"),
        "{receipt:?}"
    );

    remove_dir(&dir);
}

#[test]
fn unknown_id_at_end_of_list_starts_nothing() {
    let dir = temp_dir("c2-fix-unknown");
    let (_root, head, wt) = setup_worktree(&dir, "task_f5");
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    // 第一条规则写 marker：若任何规则启动，marker 会出现。
    let marker = dir.join("marker");
    let script = write_script(&dir, "marker.sh", &format!("touch {}", marker.display()));
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_marker".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let err = run_and_build_receipt(
        &config,
        &["rule_marker".to_owned(), "model-suggested".to_owned()],
        &wt,
        &head,
        "task_f5-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            relay_git::ReceiptError::Verify(VerifyError::UnknownRule { .. })
        ),
        "{err:?}"
    );
    // 零调用：marker 不存在。
    assert!(!marker.exists(), "未知 ID 必须在任何规则启动前拒绝");

    remove_dir(&dir);
}

#[test]
fn unauthorized_addition_during_verification_fails_with_evidence() {
    let dir = temp_dir("c2-fix-rogue");
    let (_root, head, wt) = setup_worktree(&dir, "task_f6");
    // 范围不含 rogue.txt。
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let tested = collect_diff(&wt, &head, &scope).unwrap();

    // 替身规则在验证期间创建越权文件。
    let script = write_script(
        &dir,
        "rogue.sh",
        &format!("echo rogue-content > {}/rogue.txt", wt.display()),
    );
    let config = relay_git::VerifyConfig::new(vec![relay_git::VerifyRule {
        id: "rule_rogue".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let receipt = run_and_build_receipt(
        &config,
        &["rule_rogue".to_owned()],
        &wt,
        &head,
        "task_f6-1",
        &tested,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();

    // 越权改动不通过：failed 收据 + 不含文件内容的失败证据。
    assert_eq!(receipt.status, "failed");
    let evidence = receipt.post_rejections.as_ref().expect("应有被拒证据");
    assert!(evidence.unauthorized.contains(&"rogue.txt".to_owned()));
    assert!(
        !receipt.json.contains("rogue-content"),
        "证据不得含文件内容"
    );

    remove_dir(&dir);
}
