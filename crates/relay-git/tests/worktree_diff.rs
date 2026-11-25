//! WP2-B2 差异采集契约测试：真实 worktree、按实际路径授权、拒绝分类
//! 不含内容、固定顺序补丁在干净 checkout 应用后与预期一致。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_git::{collect_diff, create_worktree, WriteScope};

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

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "relay-git-diff-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

/// 基线仓库：a.txt、b.txt、track.sh（100644）、secret.txt、.gitignore(*.log)。
fn init_repo(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    fs::write(root.join("b.txt"), "two\n").unwrap();
    fs::write(root.join("track.sh"), "echo hi\n").unwrap();
    fs::write(root.join("secret.txt"), "hidden\n").unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    git_out(
        &root,
        &[
            "add",
            "a.txt",
            "b.txt",
            "track.sh",
            "secret.txt",
            ".gitignore",
        ],
    );
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    root
}

fn main_state(root: &Path) -> (String, String, String) {
    let head = git_out(root, &["rev-parse", "HEAD"]);
    let status = git_out(root, &["status", "--porcelain", "--untracked-files=all"]);
    let index = git_out(root, &["ls-files", "-s"]);
    (head, status, index)
}

#[test]
fn violations_are_rejected_by_category_without_content() {
    let dir = temp_dir("b2-reject");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let worktree_report =
        create_worktree(&root, &dir.join("managed"), "task_d1", 1, &head).unwrap();
    let wt = &worktree_report.path;
    let scope = WriteScope::new(&[
        "a.txt",
        "b.txt",
        "src/",
        "track.sh",
        "ignored.log",
        "bin.dat",
        "link.rs",
        "sub/",
    ])
    .unwrap();

    // 合规与违规混合改动。
    fs::write(wt.join("a.txt"), "one-modified\n").unwrap();
    fs::remove_file(wt.join("b.txt")).unwrap();
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join("src/new.rs"), "new content\n").unwrap();
    fs::write(wt.join("ignored.log"), "log\n").unwrap();
    fs::write(wt.join("secret.txt"), "secret-changed\n").unwrap();
    fs::write(wt.join("bin.dat"), [1u8, 0, 2, 3]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", wt.join("link.rs")).unwrap();
    git_out(
        wt,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,deadbeefdeadbeefdeadbeefdeadbeefdeadbeef,sub/link",
        ],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(wt.join("track.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    let index_before = git_out(wt, &["ls-files", "-s"]);
    let main_before = main_state(&root);
    let err = collect_diff(wt, &head, &scope).unwrap_err();
    let rejections = match &err {
        relay_git::DiffError::Rejected(r) => r.clone(),
        other => panic!("应为拒绝错误: {other:?}"),
    };

    // 分类核对（只含路径，不含内容）。
    assert_eq!(rejections.unauthorized, vec!["secret.txt".to_owned()]);
    assert_eq!(rejections.symlinks, vec!["link.rs".to_owned()]);
    assert_eq!(rejections.gitlinks, vec!["sub/link".to_owned()]);
    assert_eq!(rejections.binaries, vec!["bin.dat".to_owned()]);
    assert_eq!(rejections.mode_changes, vec!["track.sh".to_owned()]);
    assert!(rejections.invalid_paths.is_empty());
    assert!(err.to_string().contains("越权"), "{err}");
    // 被拒内容不得出现在错误信息里。
    assert!(!err.to_string().contains("secret-changed"));
    assert!(!err.to_string().contains("log\n"));

    // 采集全程只读：worktree index 与主 checkout 不变，无部分补丁产物。
    let index_after = git_out(wt, &["ls-files", "-s"]);
    assert_eq!(index_before, index_after, "worktree index 不得被修改");
    assert_eq!(main_before, main_state(&root), "主 checkout 不得被修改");

    remove_dir(&dir);
}

#[test]
fn compliant_changes_produce_deterministic_patch_that_applies_cleanly() {
    let dir = temp_dir("b2-apply");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_d2", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["a.txt", "b.txt", "src/", "ignored.log"]).unwrap();

    fs::write(wt.join("a.txt"), "one-modified\nline two\n").unwrap();
    fs::remove_file(wt.join("b.txt")).unwrap();
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join("src/new.rs"), "new content\n").unwrap();
    fs::write(wt.join("ignored.log"), "log line\n").unwrap();

    let collected = collect_diff(wt, &head, &scope).unwrap();
    // 清单按路径排序，动作正确。
    let actions: Vec<(&str, _)> = collected
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.action))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("a.txt", relay_git::ChangeAction::Modified),
            ("b.txt", relay_git::ChangeAction::Deleted),
            ("ignored.log", relay_git::ChangeAction::Added),
            ("src/new.rs", relay_git::ChangeAction::Added),
        ]
    );
    // 固定顺序：重复采集逐字节一致。
    let again = collect_diff(wt, &head, &scope).unwrap();
    assert_eq!(collected.patch, again.patch, "补丁必须确定性");
    // 覆盖全部合规变化。
    assert!(collected.patch.contains("diff --git a/b.txt b/b.txt"));
    assert!(collected.patch.contains("new file mode 100644"));
    assert!(collected.patch.contains("+new content"));

    // 临时干净 checkout 应用补丁后与预期内容一致。
    let clean = dir.join("clean");
    git_out(
        &root,
        &[
            "clone",
            "-q",
            "--no-local",
            root.to_str().unwrap(),
            clean.to_str().unwrap(),
        ],
    );
    let patch_file = dir.join("expected.patch");
    fs::write(&patch_file, &collected.patch).unwrap();
    git_out(&clean, &["apply", patch_file.to_str().unwrap()]);
    assert_eq!(
        fs::read_to_string(clean.join("a.txt")).unwrap(),
        "one-modified\nline two\n"
    );
    assert!(!clean.join("b.txt").exists());
    assert_eq!(
        fs::read_to_string(clean.join("src/new.rs")).unwrap(),
        "new content\n"
    );
    assert_eq!(
        fs::read_to_string(clean.join("ignored.log")).unwrap(),
        "log line\n"
    );

    remove_dir(&dir);
}

#[test]
fn staged_binary_and_symlink_adds_are_rejected() {
    let dir = temp_dir("b2-fix-staged");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_f1", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["bin2.dat", "link2.rs"]).unwrap();

    // 已暂存的二进制新增：文件在盘上且经 git add 进入 index。
    fs::write(wt.join("bin2.dat"), [9u8, 0, 7]).unwrap();
    git_out(wt, &["add", "bin2.dat"]);
    // 已暂存的符号链接：cacheinfo 120000，指向不存在的位置（悬空），
    // 内容不可读也不应被读取。
    git_out(
        wt,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "120000,abcdabcdabcdabcdabcdabcdabcdabcdabcdabcd,link2.rs",
        ],
    );

    let index_before = git_out(wt, &["ls-files", "-s"]);
    let err = collect_diff(wt, &head, &scope).unwrap_err();
    let rejections = match &err {
        relay_git::DiffError::Rejected(r) => r.clone(),
        other => panic!("应为拒绝错误: {other:?}"),
    };
    assert!(
        rejections.binaries.contains(&"bin2.dat".to_owned()),
        "{rejections:?}"
    );
    assert!(
        rejections.symlinks.contains(&"link2.rs".to_owned()),
        "{rejections:?}"
    );
    // 悬空链接目标未被读取（读取会得到 ENOENT 类错误而非分类拒绝）。
    assert!(!err.to_string().contains("No such file"));
    // 拒绝即无补丁产出。
    assert!(matches!(err, relay_git::DiffError::Rejected(_)));
    let index_after = git_out(wt, &["ls-files", "-s"]);
    assert_eq!(index_before, index_after);

    remove_dir(&dir);
}

#[test]
fn tracked_only_patch_preserves_whitespace_and_applies() {
    let dir = temp_dir("b2-fix-ws");
    let root = dir.join("repo");
    fs::create_dir_all(&root).unwrap();
    git_out(&root, &["init", "-q", "-b", "main"]);
    // 基线含行尾空格、Tab、CRLF 与无末行换行的文件。
    fs::write(root.join("ws.txt"), "keep   \n\ttab\n").unwrap();
    fs::write(root.join("crlf.txt"), "p\r\nq\r\n").unwrap();
    fs::write(root.join("nonl.txt"), "abc").unwrap();
    git_out(&root, &["add", "ws.txt", "crlf.txt", "nonl.txt"]);
    git_out(&root, &["commit", "-q", "-m", "baseline"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_f2", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["ws.txt", "crlf.txt", "nonl.txt"]).unwrap();

    // 跟踪文件修改：保留行尾空格/Tab/CRLF 与无末行换行。
    fs::write(wt.join("ws.txt"), "keep  \n\ttab\t\n").unwrap();
    fs::write(wt.join("crlf.txt"), "p\r\nr\r\n").unwrap();
    fs::write(wt.join("nonl.txt"), "abd").unwrap();

    let collected = collect_diff(wt, &head, &scope).unwrap();
    // 补丁末尾保留换行、内容保留行尾空白与 CR（不做 trim）。
    assert!(collected.patch.ends_with('\n'), "补丁末尾换行被截断");
    assert!(collected.patch.contains("+keep  \n"), "行尾空格丢失");
    assert!(collected.patch.contains("+\ttab\t\n"), "行内 Tab 丢失");
    assert!(collected.patch.contains("+r\r\n"), "CRLF 的 CR 丢失");
    assert!(collected.patch.contains("\\ No newline at end of file"));

    // 干净副本应用后逐字节一致。
    let clean = dir.join("clean");
    git_out(
        &root,
        &[
            "clone",
            "-q",
            "--no-local",
            root.to_str().unwrap(),
            clean.to_str().unwrap(),
        ],
    );
    let patch_file = dir.join("tracked.patch");
    fs::write(&patch_file, &collected.patch).unwrap();
    git_out(&clean, &["apply", patch_file.to_str().unwrap()]);
    assert_eq!(
        fs::read(clean.join("ws.txt")).unwrap(),
        b"keep  \n\ttab\t\n"
    );
    assert_eq!(fs::read(clean.join("crlf.txt")).unwrap(), b"p\r\nr\r\n");
    assert_eq!(fs::read(clean.join("nonl.txt")).unwrap(), b"abd");

    remove_dir(&dir);
}

#[test]
fn crlf_and_no_final_newline_adds_are_byte_identical_after_apply() {
    let dir = temp_dir("b2-fix-crlf");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_f3", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["crlf.txt", "nonl.txt"]).unwrap();

    fs::write(wt.join("crlf.txt"), "alpha\r\nbeta\r\n").unwrap();
    fs::write(wt.join("nonl.txt"), "x\ny").unwrap();

    let collected = collect_diff(wt, &head, &scope).unwrap();
    assert!(
        collected.patch.contains("+alpha\r\n"),
        "CRLF 丢失: {}",
        collected.patch
    );
    assert!(collected.patch.contains("\\ No newline at end of file"));

    let clean = dir.join("clean");
    git_out(
        &root,
        &[
            "clone",
            "-q",
            "--no-local",
            root.to_str().unwrap(),
            clean.to_str().unwrap(),
        ],
    );
    let patch_file = dir.join("adds.patch");
    fs::write(&patch_file, &collected.patch).unwrap();
    git_out(&clean, &["apply", patch_file.to_str().unwrap()]);
    assert_eq!(
        fs::read(clean.join("crlf.txt")).unwrap(),
        b"alpha\r\nbeta\r\n"
    );
    assert_eq!(fs::read(clean.join("nonl.txt")).unwrap(), b"x\ny");

    remove_dir(&dir);
}

#[test]
fn ghost_staged_change_restored_to_base_is_dropped() {
    let dir = temp_dir("b2-fix-ghost");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_g1", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["a.txt", "b.txt"]).unwrap();

    // 场景 A：暂存修改后把文件恢复到基线字节。
    fs::write(wt.join("a.txt"), "one-edit\n").unwrap();
    git_out(wt, &["add", "a.txt"]);
    fs::write(wt.join("a.txt"), "one\n").unwrap();
    // 只有这项修改时：清单与补丁均为空。
    let collected = collect_diff(wt, &head, &scope).unwrap();
    assert!(
        collected.changes.is_empty(),
        "不得留下幽灵变更条目: {:?}",
        collected.changes
    );
    assert!(collected.patch.is_empty(), "空差异不得产出补丁");

    // 场景 B：同时有另一个真实修改（b.txt，未暂存）。
    fs::write(wt.join("b.txt"), "two-edit\n").unwrap();
    let collected = collect_diff(wt, &head, &scope).unwrap();
    let entries: Vec<(&str, _)> = collected
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.action))
        .collect();
    assert_eq!(
        entries,
        vec![("b.txt", relay_git::ChangeAction::Modified)],
        "清单只包含真实修改"
    );
    assert!(collected.patch.contains("diff --git a/b.txt b/b.txt"));
    assert!(
        !collected.patch.contains("a.txt"),
        "补丁不得包含幽灵条目: {}",
        collected.patch
    );

    // 应用补丁后：只有 b.txt 变化，a.txt 保持基线字节。
    let clean = dir.join("clean");
    git_out(
        &root,
        &[
            "clone",
            "-q",
            "--no-local",
            root.to_str().unwrap(),
            clean.to_str().unwrap(),
        ],
    );
    let patch_file = dir.join("ghost.patch");
    fs::write(&patch_file, &collected.patch).unwrap();
    git_out(&clean, &["apply", patch_file.to_str().unwrap()]);
    assert_eq!(
        fs::read_to_string(clean.join("b.txt")).unwrap(),
        "two-edit\n"
    );
    assert_eq!(fs::read_to_string(clean.join("a.txt")).unwrap(), "one\n");

    remove_dir(&dir);
}

/// A4：超过单文件上限的稀疏文件立即拒绝，不读取内容、不 OOM。
#[test]
fn oversized_sparse_file_is_rejected() {
    let dir = temp_dir("a4-sparse");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_sparse", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["a.txt", "big.bin"]).unwrap();
    let file = fs::File::create(wt.join("big.bin")).unwrap();
    file.set_len(relay_git::diff::MAX_DIFF_FILE_BYTES + 1)
        .unwrap();
    drop(file);
    let error = collect_diff(wt, &head, &scope).unwrap_err();
    match error {
        relay_git::DiffError::Rejected(rejections) => assert!(
            rejections.oversized_files.contains(&"big.bin".to_owned()),
            "{rejections:?}"
        ),
        other => panic!("expected rejection: {other:?}"),
    }
    remove_dir(&dir);
}

/// A4：新增可执行文件的 mode 保留为 100755，应用后执行位仍在。
#[test]
fn added_executable_file_keeps_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("a4-exec");
    let root = init_repo(&dir);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    let report = create_worktree(&root, &dir.join("managed"), "task_exec", 1, &head).unwrap();
    let wt = &report.path;
    let scope = WriteScope::new(&["a.txt", "run.sh"]).unwrap();
    let script = wt.join("run.sh");
    fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let collected = collect_diff(wt, &head, &scope).unwrap();
    assert!(
        collected.patch.contains("new file mode 100755"),
        "{}",
        collected.patch
    );
    let clean = dir.join("clean");
    git_out(
        &root,
        &[
            "clone",
            "-q",
            "--no-local",
            root.to_str().unwrap(),
            clean.to_str().unwrap(),
        ],
    );
    let patch_file = dir.join("exec.patch");
    fs::write(&patch_file, &collected.patch).unwrap();
    git_out(&clean, &["apply", patch_file.to_str().unwrap()]);
    let mode = fs::metadata(clean.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "执行位应在应用后保留");
    remove_dir(&dir);
}
