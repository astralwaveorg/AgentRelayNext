//! WP2-A2 只读基线预检契约测试：真实临时仓库，含 staged/unstaged 改动
//! 时 HEAD、index 与工作文件保持原样；负例矩阵覆盖分支名、缩写、大写、
//! 缺失对象、非 commit 对象、根目录不匹配与保守拒绝的仓库类型。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_git::{verify_baseline, GitCheckError};

/// 结构化 argv 运行测试夹具用的 git 命令（测试自身的 setup 允许写）。
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

/// 初始化临时仓库并提交基线文件，返回仓库根。
fn init_repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "baseline\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    root
}

fn read_only_state(root: &Path) -> (String, String, String, String) {
    let head = git_out(root, &["rev-parse", "HEAD"]);
    let status = git_out(root, &["status", "--porcelain", "--untracked-files=all"]);
    let index = git_out(root, &["ls-files", "-s"]);
    let content = fs::read_to_string(root.join("a.txt")).unwrap();
    (head, status, index, content)
}

#[test]
fn verify_baseline_is_read_only_with_staged_and_unstaged_changes() {
    let dir = temp_dir("a2-readonly");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);

    // 制造 staged、unstaged 与 untracked 改动。
    fs::write(root.join("staged.txt"), "staged\n").unwrap();
    git_out(&root, &["add", "staged.txt"]);
    fs::write(root.join("a.txt"), "modified\n").unwrap();
    fs::write(root.join("untracked.txt"), "untracked\n").unwrap();

    let before = read_only_state(&root);
    let report = verify_baseline(&root, &head).unwrap();
    assert_eq!(report.commit_oid, head);
    assert_eq!(report.object_format, "sha1");

    // HEAD、index、工作文件与状态全部原样。
    let after = read_only_state(&root);
    assert_eq!(before, after, "基线预检必须是只读的");
    assert!(
        after.1.contains(" M a.txt"),
        "unstaged 改动应保持: {}",
        after.1
    );
    assert!(
        after.1.contains("A  staged.txt"),
        "staged 改动应保持: {}",
        after.1
    );
    assert!(
        after.1.contains("?? untracked.txt"),
        "untracked 应保持: {}",
        after.1
    );
    // 重复调用结果一致。
    verify_baseline(&root, &head).unwrap();

    remove_dir(&dir);
}

#[test]
fn invalid_oid_texts_are_rejected_without_invoking_git() {
    let dir = temp_dir("a2-oid-text");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    // 分支名。
    let err = verify_baseline(&root, "main").unwrap_err();
    assert!(matches!(err, GitCheckError::InvalidOid { .. }), "{err:?}");
    // 缩写。
    let err = verify_baseline(&root, &head[..7]).unwrap_err();
    assert!(matches!(err, GitCheckError::InvalidOid { .. }), "{err:?}");
    // 大写。
    let err = verify_baseline(&root, &head.to_uppercase()).unwrap_err();
    assert!(matches!(err, GitCheckError::InvalidOid { .. }), "{err:?}");
    // 非十六进制。
    let err = verify_baseline(&root, "zzzz").unwrap_err();
    assert!(matches!(err, GitCheckError::InvalidOid { .. }), "{err:?}");
    remove_dir(&dir);
}

#[test]
fn oid_length_must_match_repo_object_format() {
    let dir = temp_dir("a2-format");
    // SHA-256 仓库（git 2.29+ 支持）。
    let root = dir.join("repo256");
    fs::create_dir_all(&root).unwrap();
    let init = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["init", "-q", "-b", "main", "--object-format=sha256"])
        .output()
        .unwrap();
    if !init.status.success() {
        // 旧版 git 不支持 sha256：跳过该场景（不视为失败）。
        eprintln!("skip: git 不支持 --object-format=sha256");
        remove_dir(&dir);
        return;
    }
    git_out(&root, &["config", "user.email", "t@example.com"]);
    git_out(&root, &["config", "user.name", "t"]);
    fs::write(root.join("a.txt"), "baseline\n").unwrap();
    git_out(&root, &["add", "a.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    assert_eq!(head.len(), 64, "sha256 仓库 HEAD 应为 64 位 OID");
    let report = verify_baseline(&root, &head).unwrap();
    assert_eq!(report.object_format, "sha256");
    // 40 位 OID 与 sha256 格式不符。
    let err = verify_baseline(&root, &"a".repeat(40)).unwrap_err();
    assert!(matches!(err, GitCheckError::InvalidOid { .. }), "{err:?}");
    remove_dir(&dir);
}

#[test]
fn missing_object_and_non_commit_are_rejected() {
    let dir = temp_dir("a2-objects");
    let root = init_repo(&dir);
    // 全零 40 位：格式正确但对象缺失。
    let err = verify_baseline(&root, &"0".repeat(40)).unwrap_err();
    assert!(
        matches!(err, GitCheckError::ObjectMissing { .. }),
        "{err:?}"
    );
    // blob 对象不是 commit。
    let blob = git_out(&root, &["hash-object", "-w", "a.txt"]);
    let err = verify_baseline(&root, &blob).unwrap_err();
    assert!(
        matches!(err, GitCheckError::NotACommit { ref kind, .. } if kind == "blob"),
        "{err:?}"
    );
    remove_dir(&dir);
}

#[test]
fn root_mismatch_and_non_repo_are_rejected() {
    let dir = temp_dir("a2-root");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    // 子目录：toplevel 不等于登记根。
    let sub = root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let err = verify_baseline(&sub, &head).unwrap_err();
    assert!(matches!(err, GitCheckError::RootMismatch { .. }), "{err:?}");
    // 非仓库目录。
    let plain = dir.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let err = verify_baseline(&plain, &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::NotARepository { .. }),
        "{err:?}"
    );
    // 不存在的目录。
    let err = verify_baseline(&dir.join("nope"), &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::NotARepository { .. }),
        "{err:?}"
    );
    remove_dir(&dir);
}

#[test]
fn unsafe_repository_kinds_are_rejected() {
    let dir = temp_dir("a2-unsafe");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);

    // partial clone。
    git_out(&root, &["config", "extensions.partialclone", "origin"]);
    let err = verify_baseline(&root, &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::UnsafeRepository { .. }),
        "{err:?}"
    );
    git_out(&root, &["config", "--unset", "extensions.partialclone"]);

    // submodule 配置。
    git_out(&root, &["config", "submodule.x.path", "x"]);
    let err = verify_baseline(&root, &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::UnsafeRepository { .. }),
        "{err:?}"
    );
    git_out(&root, &["config", "--unset", "submodule.x.path"]);

    // 自定义过滤器（LFS 形态）。
    git_out(&root, &["config", "filter.lfs.clean", "git-lfs clean"]);
    let err = verify_baseline(&root, &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::UnsafeRepository { .. }),
        "{err:?}"
    );
    git_out(&root, &["config", "--unset", "filter.lfs.clean"]);

    // replace 引用。
    git_out(
        &root,
        &["update-ref", &format!("refs/replace/{head}"), &head],
    );
    let err = verify_baseline(&root, &head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::UnsafeRepository { .. }),
        "{err:?}"
    );
    git_out(
        &root,
        &["update-ref", "-d", &format!("refs/replace/{head}")],
    );

    // 清理后恢复通过。
    verify_baseline(&root, &head).unwrap();

    remove_dir(&dir);
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("relay-git-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

#[test]
fn gitlink_in_baseline_tree_is_rejected_without_submodule_config() {
    let dir = temp_dir("a2-gitlink");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    // 手工 stage 一个 Gitlink（mode 160000）：submodule 未初始化、
    // 本地也没有任何 submodule 配置。
    git_out(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1234567890123456789012345678901234567890,sub",
        ],
    );
    git_out(&root, &["commit", "-q", "-m", "add gitlink"]);
    let new_head = git_out(&root, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, head);

    let before = read_only_state(&root);
    let err = verify_baseline(&root, &new_head).unwrap_err();
    assert!(
        matches!(err, GitCheckError::UnsafeRepository { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("Gitlink"), "{err}");
    // 预检仍不得修改 HEAD、index 或工作文件。
    let after = read_only_state(&root);
    assert_eq!(before, after, "基线预检必须是只读的");

    remove_dir(&dir);
}
