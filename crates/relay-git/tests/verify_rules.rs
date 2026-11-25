//! WP2-C1 受控验证契约测试：本地测试替身覆盖成功、非零退出、找不到
//! 工具、超时和未授权规则；工作目录为受管 worktree；不继承敏感环境。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_git::{
    create_worktree, run_rule, VerificationStatus, VerifyConfig, VerifyError, VerifyRule,
    MAX_OUTPUT_EXCERPT_BYTES,
};

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
        "relay-git-verify-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 测试替身脚本：固定 executable（可信配置引用其绝对路径）。
#[test]
fn outcomes_cover_success_failure_tool_and_timeout() {
    let dir = temp_dir("c1-outcomes");
    let root = init_base(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_c1", 1, &head).unwrap();
    let wt = report.path.clone();

    // 成功：exit 0。
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "ok".into(),
        executable: "/usr/bin/true".into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "ok", &wt).unwrap();
    assert_eq!(outcome.status, VerificationStatus::Passed);
    assert_eq!(outcome.exit_code, Some(0));
    assert!(!outcome.timed_out);

    // 非零退出。
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "fail".into(),
        executable: "/usr/bin/false".into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "fail", &wt).unwrap();
    assert_eq!(outcome.status, VerificationStatus::Failed);
    assert_eq!(outcome.exit_code, Some(1));

    // 找不到工具。
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "missing".into(),
        executable: "/nonexistent/tool-xyz".into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let err = run_rule(&config, "missing", &wt).unwrap_err();
    assert!(matches!(err, VerifyError::ToolNotFound { .. }), "{err:?}");

    // 超时（替身：/bin/sleep 5，超时 200ms）：到期终止直接子进程。
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "slow".into(),
        executable: "/bin/sleep".into(),
        argv: vec!["5".into()],
        timeout_ms: 200,
    }])
    .unwrap();
    let outcome = run_rule(&config, "slow", &wt).unwrap();
    assert_eq!(outcome.status, VerificationStatus::Timeout);
    assert!(outcome.timed_out);
    assert_eq!(outcome.exit_code, None);
    assert!(outcome.elapsed_ms < 5_000, "超时必须在时限内返回");

    remove_dir(&dir);

    fn init_base(dir: &Path) -> PathBuf {
        let root = dir.join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_out(&root, &["init", "-q", "-b", "main"]);
        git_out(&root, &["add", "a.txt"]);
        git_out(&root, &["commit", "-q", "-m", "baseline"]);
        root
    }
}

#[test]
fn workdir_is_managed_worktree_and_env_is_cleared() {
    let dir = temp_dir("c1-env");
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_c1b", 1, &head).unwrap();
    let wt = report.path.clone();

    // 替身脚本输出工作目录与敏感环境变量。
    let script = write_script(
        &dir,
        "probe.sh",
        "pwd\nif [ -n \"$SECRET_TOKEN\" ]; then echo SECRET_LEAKED=$SECRET_TOKEN; fi\necho done\n",
    );
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "probe".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "probe", &wt).unwrap();
    assert_eq!(outcome.status, VerificationStatus::Passed);
    // 工作目录是受管 worktree。
    assert!(
        outcome
            .output_excerpt
            .lines()
            .any(|line| Path::new(line) == wt.as_path()),
        "输出第一行应为 worktree 路径: {:?}",
        outcome.output_excerpt
    );
    // 敏感环境未继承。
    assert!(
        !outcome.output_excerpt.contains("SECRET_LEAKED"),
        "环境必须清空: {:?}",
        outcome.output_excerpt
    );
    assert!(outcome.output_excerpt.contains("done"));

    remove_dir(&dir);
}

#[test]
fn unknown_and_unauthorized_rules_are_rejected() {
    let dir = temp_dir("c1-auth");
    let root = init_base(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_c1c", 1, &head).unwrap();
    let wt = report.path.clone();
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "approved".into(),
        executable: "/usr/bin/true".into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();

    // 未知/未授权 ID 一律拒绝（模型给出的任何命令没有入口）。
    let err = run_rule(&config, "model-suggested-cmd", &wt).unwrap_err();
    assert!(matches!(err, VerifyError::UnknownRule { .. }), "{err:?}");
    // 配置内重复 ID 拒绝。
    let dup = VerifyConfig::new(vec![
        VerifyRule {
            id: "dup".into(),
            executable: "/usr/bin/true".into(),
            argv: vec![],
            timeout_ms: 5_000,
        },
        VerifyRule {
            id: "dup".into(),
            executable: "/usr/bin/false".into(),
            argv: vec![],
            timeout_ms: 5_000,
        },
    ])
    .unwrap_err();
    assert!(
        matches!(dup, VerifyError::DuplicateRuleId { .. }),
        "{dup:?}"
    );
    // 工作目录无效。
    let err = run_rule(&config, "approved", &dir.join("nope")).unwrap_err();
    assert!(matches!(err, VerifyError::WorkdirInvalid { .. }), "{err:?}");

    remove_dir(&dir);

    fn init_base(dir: &Path) -> PathBuf {
        let root = dir.join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_out(&root, &["init", "-q", "-b", "main"]);
        git_out(&root, &["add", "a.txt"]);
        git_out(&root, &["commit", "-q", "-m", "baseline"]);
        root
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

#[test]
fn large_output_success_excerpt_cap_and_cleaning() {
    let dir = temp_dir("c1-fix-clean");
    let root = init_base(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_c1d", 1, &head).unwrap();
    let wt = report.path.clone();

    // 替身输出 10 KiB：ANSI 颜色、BEL 控制字符、凭据行、行尾空格。
    let script = write_script(
        &dir,
        "noisy.sh",
        "for i in 1 2 3 4 5 6 7 8 9 10; do printf '\\033[31mred%d\\033[0m\\a token=abc123 secret=value%d\\n' $i $i; done\necho PLAIN_OK\n",
    );
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "noisy".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "noisy", &wt).unwrap();

    // 大量输出仍成功退出（管道满不会把正常退出误判为超时）。
    assert_eq!(outcome.status, VerificationStatus::Passed);
    assert_eq!(outcome.exit_code, Some(0));
    // 摘要上限：含截断标记按 UTF-8 字节计 ≤ 2 KiB。
    assert!(
        outcome.output_excerpt.len() <= MAX_OUTPUT_EXCERPT_BYTES,
        "摘要 {} 字节超限",
        outcome.output_excerpt.len()
    );
    // 清洗：ANSI 序列被去除，控制字符被转义，凭据值被过滤。
    assert!(!outcome.output_excerpt.contains('\u{1b}'), "ANSI 未去除");
    assert!(outcome.output_excerpt.contains("\\u{07}"), "控制字符应转义");
    assert!(!outcome.output_excerpt.contains("abc123"), "凭据未过滤");
    assert!(outcome.output_excerpt.contains("token=[已过滤]"));
    assert!(
        outcome.output_excerpt.contains("PLAIN_OK"),
        "明文应保留: {:?}",
        outcome.output_excerpt
    );
    drop(outcome);

    // executable 相对路径在配置入口即拒绝。
    let err = VerifyConfig::new(vec![VerifyRule {
        id: "relative".into(),
        executable: "cargo".into(),
        argv: vec!["test".into()],
        timeout_ms: 5_000,
    }])
    .unwrap_err();
    assert!(
        matches!(err, VerifyError::InvalidRule { ref reason, .. } if reason.contains("绝对路径")),
        "{err:?}"
    );

    remove_dir(&dir);

    fn init_base(dir: &Path) -> PathBuf {
        let root = dir.join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_out(&root, &["init", "-q", "-b", "main"]);
        git_out(&root, &["add", "a.txt"]);
        git_out(&root, &["commit", "-q", "-m", "baseline"]);
        root
    }
}

#[test]
fn credential_redaction_keeps_byte_positions_with_unicode_prefix() {
    let dir = temp_dir("c1-fix-bytes");
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_c1e", 1, &head).unwrap();
    let wt = report.path.clone();

    // 替身输出：Unicode 前缀（其小写化会改变字节长度）+ ASCII 前缀 +
    // 假值凭据；早期无关分隔符不得截断匹配。
    let script = write_script(
        &dir,
        "uni.sh",
        "printf '\\u{130}\\u{131}前缀 token = abc123\\n'\nprintf 'Authorization : Bearer xyz\\n'\necho PLAIN_OK\n",
    );
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "uni".into(),
        executable: script.to_str().unwrap().into(),
        argv: vec![],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "uni", &wt).unwrap();

    // 假值不得留在摘要中（含 Unicode 前缀行与 ASCII 行）。
    assert!(
        !outcome.output_excerpt.contains("abc123"),
        "{:?}",
        outcome.output_excerpt
    );
    assert!(
        !outcome.output_excerpt.contains("Bearer xyz"),
        "{:?}",
        outcome.output_excerpt
    );
    assert!(outcome.output_excerpt.contains("token = [已过滤]"));
    assert!(outcome.output_excerpt.contains("Authorization : [已过滤]"));
    assert!(outcome.output_excerpt.contains("PLAIN_OK"));
    // 摘要上限契约保留。
    assert!(outcome.output_excerpt.len() <= MAX_OUTPUT_EXCERPT_BYTES);

    remove_dir(&dir);
}

/// A15：常见凭据形态在收据摘要中被过滤，普通输出不受影响。
#[test]
fn credential_like_output_is_redacted() {
    let dir = temp_dir("a15-redact");
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_a15", 1, &head).unwrap();
    let wt = report.path.clone();
    let config = VerifyConfig::new(vec![VerifyRule {
        id: "leak".into(),
        executable: "/usr/bin/printf".into(),
        argv: vec![
            "normal\\ntoken = abc123secret\\nsk-abcdefgh12345678\\nghp_abcdefgh12345678\\nmonkey = value\\n"
                .into(),
        ],
        timeout_ms: 5_000,
    }])
    .unwrap();
    let outcome = run_rule(&config, "leak", &wt).unwrap();
    let excerpt = &outcome.output_excerpt;
    assert!(excerpt.contains("normal"));
    assert!(!excerpt.contains("abc123secret"), "{excerpt:?}");
    assert!(!excerpt.contains("sk-abcdefgh"), "{excerpt:?}");
    assert!(!excerpt.contains("ghp_abcdefgh"), "{excerpt:?}");
    assert!(excerpt.contains("[已过滤]"), "{excerpt:?}");
    assert!(excerpt.contains("monkey = value"), "{excerpt:?}");
    remove_dir(&dir);
}
