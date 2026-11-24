//! WP2-B3 差异制品契约测试：摘要绑定、上限拒绝、无变化不生成制品、
//! 确定性与篡改检测。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use relay_git::{
    build_artifact, publish_artifact, read_back_artifact, verify_artifact, ArtifactError,
    ArtifactInput, ChangeAction, ChangeEntry, CollectedDiff, WriteScope,
};

fn sample_collected() -> CollectedDiff {
    CollectedDiff {
        patch: "diff --git a/a.txt b/a.txt\nindex 111..222 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+one-modified\n".to_owned(),
        changes: vec![
            ChangeEntry {
                path: "a.txt".to_owned(),
                action: ChangeAction::Modified,
            },
            ChangeEntry {
                path: "src/new.rs".to_owned(),
                action: ChangeAction::Added,
            },
        ],
    }
}

fn sample_input<'a>(collected: &'a CollectedDiff) -> ArtifactInput<'a> {
    input_for("task_x1-1", collected)
}

/// 按目标 attempt 构造一致的制品输入。
fn input_for<'a>(attempt: &'a str, collected: &'a CollectedDiff) -> ArtifactInput<'a> {
    ArtifactInput {
        attempt,
        base_oid: "0123456789abcdef0123456789abcdef01234567",
        created_at: "2026-11-22T01:00:00Z",
        collected,
    }
}

#[test]
fn build_binds_digest_and_is_deterministic() {
    let collected = sample_collected();
    let artifact = build_artifact(sample_input(&collected)).unwrap();

    // manifest 绑定固定绑定项与补丁摘要/大小。
    assert!(artifact.manifest_json.contains("\"schema_version\":1"));
    assert!(artifact.manifest_json.contains("\"attempt\":\"task_x1-1\""));
    assert!(artifact
        .manifest_json
        .contains("\"base_oid\":\"0123456789abcdef0123456789abcdef01234567\""));
    assert!(artifact
        .manifest_json
        .contains("\"created_at\":\"2026-11-22T01:00:00Z\""));
    assert!(artifact
        .manifest_json
        .contains("\"path\":\"a.txt\",\"action\":\"modified\""));
    assert!(artifact
        .manifest_json
        .contains("\"path\":\"src/new.rs\",\"action\":\"added\""));

    // SHA-256 正确性：与独立计算一致（同一输入得到同一摘要）。
    let expected_sha = {
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(artifact.patch.as_bytes()).into();
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let declared = artifact
        .manifest_json
        .split("\"patch_sha256\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(declared, expected_sha);
    assert!(artifact
        .manifest_json
        .contains(&format!("\"patch_size_bytes\":{}", artifact.patch.len())));

    // 确定性：同一输入两次构建完全一致。
    let again = build_artifact(sample_input(&collected)).unwrap();
    assert_eq!(again, artifact);

    // 校验通过。
    verify_artifact(sample_input(&collected), &artifact).unwrap();
}

#[test]
fn one_byte_patch_change_fails_verification() {
    let collected = sample_collected();
    let artifact = build_artifact(sample_input(&collected)).unwrap();
    // 补丁改一个字节。
    let tampered_patch = artifact.patch.replacen("-one", "-ONE", 1);
    assert_ne!(tampered_patch, artifact.patch);
    let tampered = CollectedDiff {
        patch: tampered_patch,
        changes: collected.changes.clone(),
    };
    let err = verify_artifact(sample_input(&tampered), &artifact).unwrap_err();
    assert!(matches!(err, ArtifactError::ManifestMismatch), "{err:?}");
    // 校验器独立核对补丁摘要与 manifest 声明：manifest 不变、
    // 补丁改一个字节 → DigestMismatch（而非仅靠重建比较）。
    let tampered_artifact = relay_git::DiffArtifact {
        manifest_json: artifact.manifest_json.clone(),
        patch: artifact.patch.replacen("-one", "-onf", 1),
    };
    let err = verify_artifact(sample_input(&collected), &tampered_artifact).unwrap_err();
    assert!(
        matches!(err, ArtifactError::DigestMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn limits_are_rejected_without_truncation() {
    // 变更文件 129 个 → 超限拒绝。
    let mut many = Vec::new();
    for i in 0..129 {
        many.push(ChangeEntry {
            path: format!("f{i}.txt"),
            action: ChangeAction::Added,
        });
    }
    let collected = CollectedDiff {
        patch: "patch\n".to_owned(),
        changes: many,
    };
    let err = build_artifact(sample_input(&collected)).unwrap_err();
    assert!(
        matches!(err, ArtifactError::TooManyFiles { count: 129 }),
        "{err:?}"
    );

    // 补丁超过 512 KiB → 拒绝而不截断。
    let big_patch = "x".repeat(512 * 1024 + 1);
    let collected = CollectedDiff {
        patch: big_patch,
        changes: vec![ChangeEntry {
            path: "a.txt".to_owned(),
            action: ChangeAction::Modified,
        }],
    };
    let err = build_artifact(sample_input(&collected)).unwrap_err();
    assert!(
        matches!(err, ArtifactError::PatchTooLarge { size } if size == 512 * 1024 + 1),
        "{err:?}"
    );

    // manifest 超过 64 KiB → 拒绝（长路径撑大 manifest）。
    let mut changes = Vec::new();
    let long_path = "d".repeat(500);
    for i in 0..128 {
        changes.push(ChangeEntry {
            path: format!("{long_path}{i}.txt"),
            action: ChangeAction::Added,
        });
    }
    let collected = CollectedDiff {
        patch: "p\n".to_owned(),
        changes,
    };
    let err = build_artifact(sample_input(&collected)).unwrap_err();
    assert!(
        matches!(err, ArtifactError::ManifestTooLarge { .. }),
        "{err:?}"
    );
}

#[test]
fn no_changes_and_empty_fields_are_rejected() {
    let collected = CollectedDiff {
        patch: String::new(),
        changes: Vec::new(),
    };
    let err = build_artifact(sample_input(&collected)).unwrap_err();
    assert!(matches!(err, ArtifactError::NoChanges), "{err:?}");

    // 空绑定字段。
    let err = build_artifact(ArtifactInput {
        attempt: "",
        base_oid: "0123456789abcdef0123456789abcdef01234567",
        created_at: "2026-11-22T01:00:00Z",
        collected: &sample_collected(),
    })
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::EmptyField { field: "attempt" }),
        "{err:?}"
    );
}

#[test]
fn empty_patch_with_changes_is_rejected() {
    let collected = CollectedDiff {
        patch: String::new(),
        changes: vec![ChangeEntry {
            path: "a.txt".to_owned(),
            action: ChangeAction::Modified,
        }],
    };
    let err = build_artifact(sample_input(&collected)).unwrap_err();
    assert!(matches!(err, ArtifactError::EmptyPatch), "{err:?}");
}

#[test]
fn collect_diff_feeds_build_and_verify_end_to_end() {
    use relay_git::{collect_diff, create_worktree};
    use std::fs;
    use std::process::Command;

    let dir = std::env::temp_dir().join(format!(
        "relay-git-b3-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    let g = |args: &[&str], cwd: &Path| -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?} 失败");
        String::from_utf8_lossy(&o.stdout).trim_end().to_owned()
    };
    g(&["init", "-q", "-b", "main"], &root);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    g(&["add", "a.txt"], &root);
    g(&["commit", "-q", "-m", "baseline"], &root);
    let head = g(&["rev-parse", "HEAD"], &root);
    let report = create_worktree(&root, &dir.join("managed"), "task_b3", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["a.txt", "src/"]).unwrap();

    // 真实变化：修改 + 新增。
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join("src/new.rs"), "new content\n").unwrap();

    // 从实际 collect_diff 到 build_artifact 再到 verify 的完整链路。
    let collected = collect_diff(wt, &head, &scope).unwrap();
    let input = ArtifactInput {
        attempt: "task_b3-1",
        base_oid: &head,
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    let artifact = build_artifact(input).unwrap();
    assert!(artifact.manifest_json.contains("\"attempt\":\"task_b3-1\""));
    assert!(artifact
        .manifest_json
        .contains(&format!("\"patch_size_bytes\":{}", collected.patch.len())));
    verify_artifact(input, &artifact).unwrap();
    // 同一输入得到同一摘要。
    let again = build_artifact(input).unwrap();
    assert_eq!(again.manifest_json, artifact.manifest_json);

    let _ = fs::remove_dir_all(&dir);
}

/// git 命令输出（测试夹具）。
fn git_out(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} 失败");
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

/// 真实验证流程夹具：git 仓库 + worktree + 通过规则执行，产出
/// run_and_build_receipt 生成的真实通过收据。
fn real_pass_fixture(
    dir: &Path,
    task: &str,
    attempt: &str,
) -> (String, CollectedDiff, relay_git::TestReceipt) {
    use relay_git::{
        collect_diff, create_worktree, run_and_build_receipt, VerifyConfig, VerifyRule, WriteScope,
    };

    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), task, 1, &head).unwrap();
    let wt = report.path;
    let scope = WriteScope::new(&["a.txt"]).unwrap();
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    let collected = collect_diff(&wt, &head, &scope).unwrap();
    let script = dir.join("pass.sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let config = VerifyConfig::new(vec![VerifyRule {
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
        attempt,
        &collected,
        &scope,
        "2026-11-22T02:00:00Z",
    )
    .unwrap();
    assert_eq!(receipt.status, "pass", "夹具应为通过收据");
    (head, collected, receipt)
}

#[test]
fn publish_and_read_back_round_trip() {
    use relay_git::{publish_artifact, read_back_artifact};
    let dir = temp_dir("d1-roundtrip");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    // 正例：真实验证流程生成的收据。
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1a", "task_d1a-1");
    let input = ArtifactInput {
        attempt: "task_d1a-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };

    let handoff = r#"{"schema_version":1,"summary":"ok"}"#;
    let paths = publish_artifact(&root, "task_d1a-1", input, &receipt, handoff).unwrap();
    assert!(paths.dir.join("manifest.json").is_file());
    assert!(paths.dir.join("diff.patch").is_file());
    assert!(paths.dir.join("test_receipt.json").is_file());

    // 回读核对：attempt 目录内容与按输入重建的制品一致。
    let artifact = build_artifact(input).unwrap();
    let read_back = read_back_artifact(&root, "task_d1a-1", input, &receipt, handoff).unwrap();
    assert_eq!(read_back.manifest_json, artifact.manifest_json);
    assert_eq!(read_back.patch, collected.patch);
    remove_dir(&dir);
}

#[test]
fn duplicate_publish_is_rejected_without_overwrite() {
    use relay_git::publish_artifact;
    let dir = temp_dir("d1-dup");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1b", "task_d1b-1");
    let input = ArtifactInput {
        attempt: "task_d1b-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    let handoff = r#"{"schema_version":1,"summary":"ok"}"#;
    let first = publish_artifact(&root, "task_d1b-1", input, &receipt, handoff).unwrap();
    let before: Vec<(PathBuf, Vec<u8>)> = ["manifest.json", "diff.patch", "test_receipt.json"]
        .iter()
        .map(|name| {
            let path = first.dir.join(name);
            (path.clone(), fs::read(&path).unwrap())
        })
        .collect();

    let err = publish_artifact(&root, "task_d1b-1", input, &receipt, handoff).unwrap_err();
    assert!(
        matches!(err, ArtifactError::AlreadyPublished { .. }),
        "{err:?}"
    );
    // 三个文件字节不变（不覆盖）。
    for (path, bytes) in &before {
        assert_eq!(&fs::read(path).unwrap(), bytes, "{} 被改动", path.display());
    }

    remove_dir(&dir);
}

#[test]
fn tampered_or_incomplete_artifacts_leave_no_verifiable_set() {
    let dir = temp_dir("d1-tamper");
    let root = dir.join("artifacts");
    let collected = sample_collected();
    let receipt = relay_git::TestReceipt {
        attempt: "task_t1".to_owned(),
        base_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        diff_sha256: Some(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_owned(),
        ),
        status: "pass",
        reason: None,
        rules: Vec::new(),
        post_rejections: None,
        json: "{\"schema_version\":1}".to_owned(),
    };

    // 不同补丁的收据：发布即拒绝。
    fs::create_dir_all(&root).unwrap();
    let err = publish_artifact(
        &root,
        "task_t1",
        input_for("task_t1", &collected),
        &receipt,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ReceiptDigestMismatch { .. }),
        "{err:?}"
    );
    assert!(
        !root.join("task_t1").exists(),
        "拒绝后不得留下 attempt 目录"
    );

    // 未通过收据：发布即拒绝。
    let fail_receipt = relay_git::TestReceipt {
        attempt: "task_t2".to_owned(),
        status: "failed",
        ..receipt.clone()
    };
    let err = publish_artifact(
        &root,
        "task_t2",
        input_for("task_t2", &collected),
        &fail_receipt,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ReceiptNotPassed { .. }),
        "{err:?}"
    );
    assert!(!root.join("task_t2").exists());

    // 收据与发布 attempt 绑定不一致：发布即拒绝。
    let wrong_attempt = relay_git::TestReceipt {
        attempt: "task_other-9".to_owned(),
        diff_sha256: Some({
            use sha2::{Digest, Sha256};
            let digest: [u8; 32] = Sha256::digest(collected.patch.as_bytes()).into();
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        }),
        ..receipt.clone()
    };
    let err = publish_artifact(
        &root,
        "task_t2b",
        input_for("task_t2b", &collected),
        &wrong_attempt,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ReceiptMismatch { .. }),
        "{err:?}"
    );
    assert!(!root.join("task_t2b").exists());

    // 收据与制品基线不一致：发布即拒绝。
    let wrong_base = relay_git::TestReceipt {
        attempt: "task_t2c".to_owned(),
        base_oid: "ffffffffffffffffffffffffffffffffffffffff".to_owned(),
        diff_sha256: Some({
            use sha2::{Digest, Sha256};
            let digest: [u8; 32] = Sha256::digest(collected.patch.as_bytes()).into();
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        }),
        ..receipt.clone()
    };
    let err = publish_artifact(
        &root,
        "task_t2c",
        input_for("task_t2c", &collected),
        &wrong_base,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ReceiptMismatch { .. }),
        "{err:?}"
    );
    assert!(!root.join("task_t2c").exists());

    // 部分写入（缺 manifest）的 attempt 目录：回读即拒绝，无部分验收。
    fs::create_dir_all(root.join("task_t3")).unwrap();
    fs::write(root.join("task_t3/diff.patch"), "patch\n").unwrap();
    let err = read_back_artifact(
        &root,
        "task_t3",
        input_for("task_t3", &collected),
        &receipt,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::MissingArtifactItem { .. }),
        "{err:?}"
    );

    // 篡改补丁字节后回读：摘要不符拒绝。
    let artifact = build_artifact(input_for("task_t4", &collected)).unwrap();
    fs::create_dir_all(root.join("task_t4")).unwrap();
    fs::write(root.join("task_t4/manifest.json"), &artifact.manifest_json).unwrap();
    fs::write(
        root.join("task_t4/diff.patch"),
        artifact.patch.replace("-one", "-ONE"),
    )
    .unwrap();
    fs::write(
        root.join("task_t4/test_receipt.json"),
        "{\"schema_version\":1}",
    )
    .unwrap();
    fs::write(root.join("task_t4/handoff.json"), "{}").unwrap();
    let err = read_back_artifact(
        &root,
        "task_t4",
        input_for("task_t4", &collected),
        &receipt,
        "{}",
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::DigestMismatch { .. }),
        "{err:?}"
    );

    // 符号链接目标拒绝。
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("elsewhere"), root.join("task_t5")).unwrap();
    #[cfg(unix)]
    {
        let err = publish_artifact(
            &root,
            "task_t5",
            input_for("task_t5", &collected),
            &receipt,
            "{}",
        )
        .unwrap_err();
        assert!(
            matches!(err, ArtifactError::SymlinkRejected { .. }),
            "{err:?}"
        );
    }

    remove_dir(&dir);
}

#[test]
fn tampered_on_disk_receipt_content_is_rejected() {
    use relay_git::{publish_artifact, read_back_artifact};
    let dir = temp_dir("d1-receipt-tamper");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1c", "task_d1c-1");
    let input = ArtifactInput {
        attempt: "task_d1c-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    publish_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap();
    let receipt_path = root.join("task_d1c-1/test_receipt.json");
    let pristine = fs::read_to_string(&receipt_path).unwrap();

    // 错配一：落盘收据状态被改（首个 "status" = 顶层状态）。
    let tampered = pristine.replacen("\"status\":\"pass\"", "\"status\":\"failed\"", 1);
    assert_ne!(tampered, pristine, "夹具应含顶层 pass 状态");
    fs::write(&receipt_path, &tampered).unwrap();
    let err = read_back_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptNotPassed { status } if status == "failed"),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 错配二：落盘收据摘要换成其他补丁的摘要（整体替换键值对，保持
    // JSON 合法）。
    let real_sha = receipt.diff_sha256.clone().unwrap();
    let sha_pair = format!("\"diff_sha256\":\"{real_sha}\"");
    assert!(pristine.contains(&sha_pair));
    let tampered = pristine.replacen(
        &sha_pair,
        "\"diff_sha256\":\"deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef\"",
        1,
    );
    assert_ne!(tampered, pristine);
    fs::write(&receipt_path, &tampered).unwrap();
    let err = read_back_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptDigestMismatch { actual, .. }
            if actual.starts_with("deadbeef")),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 错配三：规则结果被改（第二个 "status" = rules 条目内）。
    let marker = "\"status\":\"pass\"";
    let first = pristine.find(marker).unwrap();
    let second = pristine[first + 1..].find(marker).unwrap() + first + 1;
    let tampered = format!(
        "{}\"status\":\"fail\"{}",
        &pristine[..second],
        &pristine[second + marker.len()..]
    );
    fs::write(&receipt_path, &tampered).unwrap();
    let err = read_back_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptMismatch { detail }
            if detail.contains("rule_pass") && detail.contains("fail")),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 错配四：落盘收据的 attempt 绑定被改。
    let tampered = pristine.replacen(
        "\"attempt\":\"task_d1c-1\"",
        "\"attempt\":\"task_other-1\"",
        1,
    );
    assert_ne!(tampered, pristine);
    fs::write(&receipt_path, &tampered).unwrap();
    let err = read_back_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptMismatch { detail }
            if detail.contains("task_other-1")),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 还原后回读恢复正常（篡改隔离在单文件，未污染其他项）。
    read_back_artifact(&root, "task_d1c-1", input, &receipt, "{}").unwrap();
    remove_dir(&dir);
}

#[test]
fn oversized_files_are_rejected_on_readback() {
    use relay_git::{publish_artifact, read_back_artifact};
    let dir = temp_dir("d1-oversize");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1d", "task_d1d-1");
    let input = ArtifactInput {
        attempt: "task_d1d-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    publish_artifact(&root, "task_d1d-1", input, &receipt, "{}").unwrap();
    let receipt_path = root.join("task_d1d-1/test_receipt.json");
    let manifest_path = root.join("task_d1d-1/manifest.json");
    let patch_path = root.join("task_d1d-1/diff.patch");
    let pristine_receipt = fs::read(&receipt_path).unwrap();
    let pristine_manifest = fs::read(&manifest_path).unwrap();
    let pristine_patch = fs::read(&patch_path).unwrap();

    // 补丁超过 512 KiB：先按大小拒绝（而非读入后再算摘要）。
    let mut big = pristine_patch.clone();
    big.extend(std::iter::repeat_n(b'x', 512 * 1024));
    fs::write(&patch_path, &big).unwrap();
    let err = read_back_artifact(&root, "task_d1d-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::PatchTooLarge { size } if size > 512 * 1024),
        "{err:?}"
    );
    fs::write(&patch_path, &pristine_patch).unwrap();

    // manifest 超过 64 KiB。
    let mut big = pristine_manifest.clone();
    big.extend(std::iter::repeat_n(b'x', 64 * 1024));
    fs::write(&manifest_path, &big).unwrap();
    let err = read_back_artifact(&root, "task_d1d-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::ManifestTooLarge { size } if size > 64 * 1024),
        "{err:?}"
    );
    fs::write(&manifest_path, &pristine_manifest).unwrap();

    // 收据超过 64 KiB。
    let mut big = pristine_receipt.clone();
    big.extend(std::iter::repeat_n(b'x', 64 * 1024));
    fs::write(&receipt_path, &big).unwrap();
    let err = read_back_artifact(&root, "task_d1d-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::ReceiptTooLarge { size } if size > 64 * 1024),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine_receipt).unwrap();

    // 还原后回读恢复通过。
    read_back_artifact(&root, "task_d1d-1", input, &receipt, "{}").unwrap();
    remove_dir(&dir);
}

#[test]
fn attempt_mismatch_is_rejected_in_publish_and_readback() {
    use relay_git::{publish_artifact, read_back_artifact};
    let dir = temp_dir("d1-attempt-mismatch");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1e", "task_d1e-1");
    // ArtifactInput attempt 与目标目录 attempt 不一致：发布即拒绝。
    let mismatched = ArtifactInput {
        attempt: "task_other-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    let err = publish_artifact(&root, "task_d1e-1", mismatched, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::AttemptMismatch { ref target, ref input }
            if target == "task_d1e-1" && input == "task_other-1"),
        "{err:?}"
    );
    assert!(!root.join("task_d1e-1").exists());

    // 正常发布后，回读同样核对 attempt 一致性。
    let input = ArtifactInput {
        attempt: "task_d1e-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    publish_artifact(&root, "task_d1e-1", input, &receipt, "{}").unwrap();
    let err = read_back_artifact(&root, "task_d1e-1", mismatched, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::AttemptMismatch { .. }),
        "{err:?}"
    );
    read_back_artifact(&root, "task_d1e-1", input, &receipt, "{}").unwrap();

    remove_dir(&dir);
}

#[test]
fn truncated_or_duplicate_key_receipts_are_rejected() {
    use relay_git::{publish_artifact, read_back_artifact};
    let dir = temp_dir("d1-receipt-json");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1f", "task_d1f-1");
    let input = ArtifactInput {
        attempt: "task_d1f-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    publish_artifact(&root, "task_d1f-1", input, &receipt, "{}").unwrap();
    let receipt_path = root.join("task_d1f-1/test_receipt.json");
    let pristine = fs::read_to_string(&receipt_path).unwrap();

    // 截断收据：结构化解码拒绝。
    let truncated = pristine[..pristine.len() / 2].to_owned();
    fs::write(&receipt_path, &truncated).unwrap();
    let err = read_back_artifact(&root, "task_d1f-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptMismatch { detail } if detail.contains("解码失败")),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 顶层重复关键字段（两个 "status"）：拒绝。
    let duplicated = pristine.replacen(
        "{\"schema_version\":1,",
        "{\"status\":\"pass\",\"schema_version\":1,",
        1,
    );
    assert_ne!(duplicated, pristine);
    fs::write(&receipt_path, &duplicated).unwrap();
    let err = read_back_artifact(&root, "task_d1f-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(&err, ArtifactError::ReceiptMismatch { detail } if detail.contains("重复")),
        "{err:?}"
    );
    fs::write(&receipt_path, &pristine).unwrap();

    // 还原后回读恢复通过。
    read_back_artifact(&root, "task_d1f-1", input, &receipt, "{}").unwrap();
    remove_dir(&dir);
}

#[test]
fn pre_existing_empty_target_dir_is_never_replaced() {
    use relay_git::publish_artifact;
    let dir = temp_dir("d1-empty-dir");
    let root = dir.join("artifacts");
    fs::create_dir_all(&root).unwrap();
    let (head, collected, receipt) = real_pass_fixture(&dir, "task_d1g", "task_d1g-1");
    let input = ArtifactInput {
        attempt: "task_d1g-1",
        base_oid: head.as_str(),
        created_at: "2026-11-22T02:00:00Z",
        collected: &collected,
    };
    // 预先存在的空目标目录：发布拒绝且目录不被替换（仍为空）。
    fs::create_dir_all(root.join("task_d1g-1")).unwrap();
    let err = publish_artifact(&root, "task_d1g-1", input, &receipt, "{}").unwrap_err();
    assert!(
        matches!(err, ArtifactError::AlreadyPublished { .. }),
        "{err:?}"
    );
    let entries: Vec<_> = fs::read_dir(root.join("task_d1g-1")).unwrap().collect();
    assert!(entries.is_empty(), "空目录不得被填充或替换: {entries:?}");

    remove_dir(&dir);
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-git-art-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}
