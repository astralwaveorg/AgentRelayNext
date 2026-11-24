//! WP2-B1 受管 worktree 契约测试：真实临时仓库，验证固定基线检出、
//! 主 checkout 不变、目标拒绝矩阵与 hooks/过滤器不执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_git::{create_worktree, GitCheckError, WorktreeError};

fn git_out(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} 启动失败: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

fn init_repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "baseline\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    root
}

fn main_state(root: &Path) -> (String, String, String, String) {
    let head = git_out(root, &["rev-parse", "HEAD"]);
    let status = git_out(root, &["status", "--porcelain", "--untracked-files=all"]);
    let index = git_out(root, &["ls-files", "-s"]);
    let content = fs::read_to_string(root.join("a.txt")).unwrap();
    (head, status, index, content)
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-git-wt-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn create_worktree_checks_out_fixed_base_and_leaves_main_untouched() {
    let dir = temp_dir("b1-happy");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    // 主 checkout 制造 staged、unstaged 与 untracked 改动。
    fs::write(root.join("staged.txt"), "staged\n").unwrap();
    git_out(&root, &["add", "staged.txt"]);
    fs::write(root.join("a.txt"), "modified\n").unwrap();
    fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    let before = main_state(&root);

    let managed = dir.join("managed");
    let report = create_worktree(&root, &managed, "task_x1", 1, &head).unwrap();
    let expected = managed
        .canonicalize()
        .unwrap()
        .join("worktrees")
        .join("task_x1")
        .join("attempt-1");
    assert_eq!(report.path, expected);
    assert_eq!(report.base_oid, head);
    assert_eq!(report.head_oid, head);

    // 固定基线：worktree 内文件是基线内容（不含主 checkout 的改动）。
    assert_eq!(
        fs::read_to_string(report.path.join("a.txt")).unwrap(),
        "baseline\n"
    );
    assert!(!report.path.join("untracked.txt").exists());
    // detached 且指向固定基线。
    assert_eq!(git_out(&report.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_out(&report.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );

    // 主 checkout 的 HEAD、index、未提交文件不变。
    let after = main_state(&root);
    assert_eq!(before, after, "主 checkout 必须保持原样");
    // 对象库与 refs 共用：worktree 内可见基线对象与主 checkout 相同。
    assert_eq!(
        git_out(&report.path, &["rev-parse", "main"]),
        git_out(&root, &["rev-parse", "main"])
    );

    // 已存在的目标拒绝，不复用、不覆盖。
    let err = create_worktree(&root, &managed, "task_x1", 1, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::TargetInvalid { ref reason, .. } if reason.contains("已存在")),
        "{err:?}"
    );

    remove_dir(&dir);
}

#[test]
fn precheck_failure_prevents_worktree_creation() {
    let dir = temp_dir("b1-precheck");
    let root = init_repo(&dir);
    let managed = dir.join("managed");
    let missing = "0".repeat(40);
    let err = create_worktree(&root, &managed, "task_y1", 1, &missing).unwrap_err();
    assert!(matches!(err, WorktreeError::Check(_)), "{err:?}");
    // 目标未被创建。
    assert!(!managed.join("worktrees").exists(), "预检失败不得创建目录");

    // 过滤器与 partial clone 仓库在预检即拒绝。
    git_out(&root, &["config", "filter.lfs.clean", "git-lfs clean"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let err = create_worktree(&root, &managed, "task_y2", 1, &head).unwrap_err();
    assert!(
        matches!(
            err,
            WorktreeError::Check(GitCheckError::UnsafeRepository { .. })
        ),
        "{err:?}"
    );
    assert!(!managed.join("worktrees").join("task_y2").exists());

    remove_dir(&dir);
}

#[test]
fn invalid_task_id_and_symlinked_targets_are_rejected() {
    let dir = temp_dir("b1-targets");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let managed = dir.join("managed");

    // task_id 逃逸与形状非法。
    for bad in ["../escape", "a/b", "", ".hidden", "a\\b"] {
        let err = create_worktree(&root, &managed, bad, 1, &head).unwrap_err();
        assert!(
            matches!(err, WorktreeError::InvalidRequest { .. }),
            "{bad}: {err:?}"
        );
    }
    // attempt 编号必须为正。
    let err = create_worktree(&root, &managed, "task_z", 0, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::InvalidRequest { .. }),
        "{err:?}"
    );

    // 符号链接组件拒绝：受管根下的 worktrees 目录被替换为符号链接。
    fs::create_dir_all(managed.join("worktrees")).unwrap();
    let elsewhere = dir.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let worktrees = managed.join("worktrees");
    fs::remove_dir(&worktrees).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&elsewhere, &worktrees).unwrap();
    let err = create_worktree(&root, &managed, "task_z", 1, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::TargetInvalid { ref reason, .. } if reason.contains("符号链接")),
        "{err:?}"
    );
    assert!(!elsewhere.join("worktrees").join("task_z").exists());

    remove_dir(&dir);
}

#[test]
fn hooks_and_filters_do_not_run() {
    let dir = temp_dir("b1-hooks");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    // 配置指向带可执行 post-checkout 钩子的目录：钩子会写标记文件。
    let hooks_dir = dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let marker = hooks_dir.join("marker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            hooks_dir.join("post-checkout"),
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(
            hooks_dir.join("post-checkout"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    git_out(
        &root,
        &["config", "core.hooksPath", hooks_dir.to_str().unwrap()],
    );

    let managed = dir.join("managed");
    let report = create_worktree(&root, &managed, "task_h1", 1, &head).unwrap();
    // 钩子未执行：无标记文件。
    assert!(!marker.exists(), "post-checkout 钩子不得执行");
    // worktree 本身检出正常。
    assert!(report.path.join("a.txt").exists());
    drop(report);

    // 配置过滤器（LFS 形态）的仓库直接拒绝，不创建。
    git_out(&root, &["config", "filter.lfs.smudge", "git-lfs smudge"]);
    let err = create_worktree(&root, &managed, "task_h2", 1, &head).unwrap_err();
    assert!(
        matches!(
            err,
            WorktreeError::Check(GitCheckError::UnsafeRepository { .. })
        ),
        "{err:?}"
    );
    assert!(!managed.join("worktrees").join("task_h2").exists());

    remove_dir(&dir);
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

#[test]
fn managed_root_inside_main_checkout_is_rejected_without_side_effects() {
    let dir = temp_dir("b1-fix-inside");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    fs::write(root.join("a.txt"), "modified\n").unwrap();
    fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    let before = main_state(&root);

    // 受管根是主 checkout 内的普通目录（预创建、空）。
    fs::create_dir_all(root.join("managed-inside")).unwrap();
    let err =
        create_worktree(&root, &root.join("managed-inside"), "task_m1", 1, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::TargetInvalid { ref reason, .. } if reason.contains("主 checkout")),
        "{err:?}"
    );
    // 拒绝后目录仍为空，主 checkout 状态不变。
    assert!(
        fs::read_dir(root.join("managed-inside"))
            .unwrap()
            .next()
            .is_none(),
        "拒绝后受管目录不得有任何内容"
    );
    assert_eq!(main_state(&root), before, "主 checkout 必须保持原样");
    remove_dir(&dir);
}

#[test]
fn symlinked_managed_root_is_rejected_and_link_target_stays_empty() {
    let dir = temp_dir("b1-fix-symlink");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    use std::os::unix::fs::symlink;

    // 场景 1：受管根本身是符号链接，指向 checkout 外的空目录。
    let outside = dir.join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, root.join("managed-link")).unwrap();
    // 场景 2 的 setup 也在基线采集之前完成。
    fs::create_dir_all(root.join("inside-empty")).unwrap();
    symlink(root.join("inside-empty"), root.join("managed-link2")).unwrap();
    // 状态基线在全部 setup 完成后采集（链接本身是 setup 的一部分）。
    let before = main_state(&root);

    // 场景 1：受管根本身是符号链接 → 拒绝，链接目标仍为空。
    let err = create_worktree(&root, &root.join("managed-link"), "task_m2", 1, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::TargetInvalid { ref reason, .. } if reason.contains("符号链接")),
        "{err:?}"
    );
    assert!(
        fs::read_dir(&outside).unwrap().next().is_none(),
        "链接目标不得被写入"
    );
    assert_eq!(main_state(&root), before);

    // 场景 2：受管根解析后落在主 checkout 内 → 拒绝，目标目录仍为空。
    let err = create_worktree(&root, &root.join("managed-link2"), "task_m3", 1, &head).unwrap_err();
    assert!(
        matches!(err, WorktreeError::TargetInvalid { .. }),
        "{err:?}"
    );
    assert!(
        fs::read_dir(root.join("inside-empty"))
            .unwrap()
            .next()
            .is_none(),
        "链接目标不得被写入"
    );
    assert_eq!(main_state(&root), before);

    // 对照：checkout 外的真实受管根不受影响，仍可创建。
    let managed = dir.join("managed");
    let report = create_worktree(&root, &managed, "task_ok", 1, &head).unwrap();
    assert!(report.path.join("a.txt").exists());

    remove_dir(&dir);
}
