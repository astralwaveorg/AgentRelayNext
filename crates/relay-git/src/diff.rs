//! 受管 worktree 相对固定基线的真实变化采集（06「工作副本生命周期」§4）。
//!
//! 不依赖模型声明、不信任任何 index：以 `git diff --raw <基线>` 覆盖
//! staged、unstaged 与删除，以 `git ls-files --others`（含被忽略文件）
//! 枚举新增路径。按实际路径做授权检查（两层授权交集后的有效范围）；
//! 越权、符号链接、Gitlink、二进制或模式变化返回可区分的拒绝（只含
//! 路径，不含被拒内容），任何拒绝都不产出补丁。合规文本变化生成固定
//! 顺序补丁：`git diff` 按路径序输出跟踪变化，新增文件 hunks 按路径序
//! 追加。全程只读：不改主 checkout、不改任何 index；diff 关闭外部
//! 差异程序与重命名检测（重命名表现为删除/新增，两侧分别合规）。

use std::fmt;
use std::path::Path;

use crate::git_layer::{git_out, git_raw, git_stdin};
use crate::scope::WriteScope;
use crate::GitCheckError;

/// 变化动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    /// 基线中不存在的新文件。
    Added,
    /// 相对基线内容有变化。
    Modified,
    /// 相对基线被删除。
    Deleted,
}

/// 合规变化清单条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEntry {
    /// 仓库相对路径。
    pub path: String,
    pub action: ChangeAction,
}

/// 采集结果：合规变化的固定顺序补丁与清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedDiff {
    /// 统一格式补丁（UTF-8 文本；跟踪变化在前，新增文件按路径序追加）。
    pub patch: String,
    /// 合规变化清单（按路径排序）。
    pub changes: Vec<ChangeEntry>,
}

/// 采集拒绝：可区分的类别，只列路径不返回内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejections {
    /// 越权路径（两层授权交集不覆盖）。
    pub unauthorized: Vec<String>,
    /// 路径本身不合法（如绝对路径、`.`/`..`/`.git` 组件）。
    pub invalid_paths: Vec<String>,
    /// 符号链接。
    pub symlinks: Vec<String>,
    /// Gitlink（mode 160000）。
    pub gitlinks: Vec<String>,
    /// 二进制内容（含 NUL 或非 UTF-8）。
    pub binaries: Vec<String>,
    /// 文件模式变化（如 100644 → 100755）。
    pub mode_changes: Vec<String>,
}

impl fmt::Display for Rejections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        let mut sep = |f: &mut fmt::Formatter<'_>| -> fmt::Result {
            if first {
                first = false;
                Ok(())
            } else {
                writeln!(f)
            }
        };
        let list = |paths: &[String]| -> String {
            const MAX: usize = 16;
            if paths.len() <= MAX {
                paths.join(", ")
            } else {
                format!("{}…（共 {} 条）", paths[..MAX].join(", "), paths.len())
            }
        };
        if !self.unauthorized.is_empty() {
            sep(f)?;
            write!(
                f,
                "越权路径（不在写入范围内）：{}",
                list(&self.unauthorized)
            )?;
        }
        if !self.invalid_paths.is_empty() {
            sep(f)?;
            write!(f, "非法路径：{}", list(&self.invalid_paths))?;
        }
        if !self.symlinks.is_empty() {
            sep(f)?;
            write!(f, "符号链接：{}", list(&self.symlinks))?;
        }
        if !self.gitlinks.is_empty() {
            sep(f)?;
            write!(f, "Gitlink（mode 160000）：{}", list(&self.gitlinks))?;
        }
        if !self.binaries.is_empty() {
            sep(f)?;
            write!(f, "二进制或非 UTF-8 内容：{}", list(&self.binaries))?;
        }
        if !self.mode_changes.is_empty() {
            sep(f)?;
            write!(f, "文件模式变化：{}", list(&self.mode_changes))?;
        }
        Ok(())
    }
}

/// 采集错误。
#[derive(Debug)]
pub enum DiffError {
    /// 存在拒绝项：不返回被拒内容，也不产出部分补丁。
    Rejected(Box<Rejections>),
    /// Git 调用失败。
    Git(GitCheckError),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffError::Rejected(r) => write!(f, "变化采集存在拒绝项：{r}"),
            DiffError::Git(e) => write!(f, "差异采集 Git 调用失败：{e}"),
        }
    }
}

impl std::error::Error for DiffError {}

impl From<GitCheckError> for DiffError {
    fn from(e: GitCheckError) -> Self {
        DiffError::Git(e)
    }
}

impl From<std::io::Error> for DiffError {
    fn from(e: std::io::Error) -> Self {
        DiffError::Git(GitCheckError::GitInvocation(format!(
            "差异采集文件读取失败：{e}"
        )))
    }
}

/// 内容是否按二进制处理：含 NUL 字节或不是合法 UTF-8（与 Git 的启发
/// 一致的保守子集）。
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || String::from_utf8(bytes.to_vec()).is_err()
}

/// 单条原始变化：模式与状态（-z 解析结果）。
struct RawChange {
    old_mode: String,
    new_mode: String,
    status: char,
    path: String,
}

/// 从受管 worktree 采集相对固定基线的真实变化。
///
/// 枚举（只读）：`git diff --raw` 覆盖 staged、unstaged 与删除；
/// `git ls-files --others`（标准 + 被忽略）覆盖新增文件。随后逐路径
/// 授权与安全检查，任一拒绝项存在即返回 [`DiffError::Rejected`]（此时
/// 不产出补丁）；全部合规时生成固定顺序补丁并返回变化清单。全程不改
/// 主 checkout、不改任何 index（共享层已关可选锁）。
pub fn collect_diff(
    worktree: &Path,
    base_oid: &str,
    scope: &WriteScope,
) -> Result<CollectedDiff, DiffError> {
    // 1. 跟踪变化（相对固定基线；关闭重命名检测与外部差异程序）。
    //    base↔worktree 是最终差异的权威来源（staged 后又恢复为基线字节
    //    的路径不产生幽灵条目）；base↔index 仅用于拒绝性检查——只存在
    //    于 index 的 Gitlink/符号链接仍要拒绝，但 index 中的旧文本变化
    //    不并入最终清单。
    let worktree_side = git_out(
        worktree,
        &[
            "-c",
            "diff.renames=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--raw",
            "-z",
            base_oid,
        ],
    )?;
    let index_side = git_out(
        worktree,
        &[
            "-c",
            "diff.renames=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--cached",
            "--raw",
            "-z",
            base_oid,
        ],
    )?;
    let listings = [worktree_side, index_side];
    let mut tracked: Vec<RawChange> = Vec::new();
    let mut staged_only: Vec<RawChange> = Vec::new();
    for (position, listing) in listings.into_iter().enumerate() {
        let mut fields = listing.split('\0').filter(|field| !field.is_empty());
        while let Some(meta) = fields.next() {
            let path = fields.next().ok_or_else(|| {
                DiffError::Git(GitCheckError::UnexpectedOutput {
                    detail: "diff --raw 输出字段不完整".into(),
                })
            })?;
            let meta = meta.strip_prefix(':').unwrap_or(meta);
            let parts: Vec<&str> = meta.split_whitespace().collect();
            if parts.len() < 5 {
                return Err(DiffError::Git(GitCheckError::UnexpectedOutput {
                    detail: format!("diff --raw 元数据异常：{meta}"),
                }));
            }
            let change = RawChange {
                old_mode: parts[0].to_owned(),
                new_mode: parts[1].to_owned(),
                status: parts[4].chars().next().unwrap_or('M'),
                path: path.to_owned(),
            };
            if position == 0 {
                if !tracked.iter().any(|c| c.path == change.path) {
                    tracked.push(change);
                }
            } else if !tracked.iter().any(|c| c.path == change.path)
                && !staged_only.iter().any(|c| c.path == change.path)
            {
                staged_only.push(change);
            }
        }
    }

    // 2. 新增文件：未跟踪（标准）与被忽略。
    let others = git_out(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let ignored = git_out(
        worktree,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let mut added_paths: Vec<String> = Vec::new();
    for listing in [others, ignored] {
        for path in listing.split('\0').filter(|p| !p.is_empty()) {
            if !added_paths.iter().any(|p| p == path) {
                added_paths.push(path.to_owned());
            }
        }
    }

    // 3. 授权与安全检查（逐实际路径；只收集路径，不读越权内容）。
    let mut rejections = Rejections {
        unauthorized: Vec::new(),
        invalid_paths: Vec::new(),
        symlinks: Vec::new(),
        gitlinks: Vec::new(),
        binaries: Vec::new(),
        mode_changes: Vec::new(),
    };
    // 仅存在于 index 的条目：Gitlink/符号链接仍拒绝；其余（如已暂存又
    // 恢复为基线字节的旧文本变化）不并入最终清单。
    for change in &staged_only {
        if change.old_mode == "160000" || change.new_mode == "160000" {
            rejections.gitlinks.push(change.path.clone());
        } else if change.new_mode == "120000" {
            rejections.symlinks.push(change.path.clone());
        }
    }
    for change in &tracked {
        if authorize(scope, &change.path, &mut rejections).is_err() {
            continue;
        }
        if change.old_mode == "160000" || change.new_mode == "160000" {
            rejections.gitlinks.push(change.path.clone());
            continue;
        }
        match change.status {
            'D' => {}
            _ => {
                // 已暂存的符号链接（mode 120000）按符号链接拒绝：
                // 只看模式与 symlink_metadata，不读取链接目标。
                if change.new_mode == "120000" {
                    rejections.symlinks.push(change.path.clone());
                    continue;
                }
                if change.status != 'A' && change.old_mode != change.new_mode {
                    rejections.mode_changes.push(change.path.clone());
                    continue;
                }
                let bytes = match fs_bytes(worktree, &change.path) {
                    Some(bytes) => bytes,
                    None => continue,
                };
                if looks_binary(&bytes) {
                    rejections.binaries.push(change.path.clone());
                }
            }
        }
    }
    for path in &added_paths {
        if authorize(scope, path, &mut rejections).is_err() {
            continue;
        }
        let bytes = match fs_bytes(worktree, path) {
            Some(bytes) => bytes,
            None => continue,
        };
        let meta = std::fs::symlink_metadata(worktree.join(path)).map_err(DiffError::from)?;
        if meta.is_symlink() {
            rejections.symlinks.push(path.clone());
            continue;
        }
        if looks_binary(&bytes) {
            rejections.binaries.push(path.clone());
        }
    }
    if !rejections.unauthorized.is_empty()
        || !rejections.invalid_paths.is_empty()
        || !rejections.symlinks.is_empty()
        || !rejections.gitlinks.is_empty()
        || !rejections.binaries.is_empty()
        || !rejections.mode_changes.is_empty()
    {
        return Err(DiffError::Rejected(Box::new(rejections)));
    }

    // 4. 合规变化清单与固定顺序补丁。
    let mut changes: Vec<ChangeEntry> = Vec::new();
    let mut tracked_paths: Vec<&str> = Vec::new();
    for change in &tracked {
        let action = match change.status {
            'D' => ChangeAction::Deleted,
            'A' => ChangeAction::Added,
            _ => ChangeAction::Modified,
        };
        tracked_paths.push(&change.path);
        changes.push(ChangeEntry {
            path: change.path.clone(),
            action,
        });
    }
    for path in &added_paths {
        changes.push(ChangeEntry {
            path: path.clone(),
            action: ChangeAction::Added,
        });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));

    let mut patch = String::new();
    let tracked_only: Vec<&str> = tracked.iter().map(|c| c.path.as_str()).collect();
    if !tracked_only.is_empty() {
        let mut args: Vec<&str> = vec![
            "-c",
            "diff.renames=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            base_oid,
            "--",
        ];
        args.extend(tracked_only.iter().copied());
        // 补丁取原始 stdout：保留末尾换行、行尾空格与 Tab，不做 trim。
        let (ok, _code, stdout, stderr) = git_raw(worktree, &args)?;
        if !ok {
            return Err(DiffError::Git(GitCheckError::CommandFailed {
                argv: format!("git {}", args.join(" ")),
                stderr: crate::git_layer::truncate(&stderr),
            }));
        }
        patch.push_str(&stdout);
    }
    let mut synthesized_adds: Vec<&String> = added_paths.iter().collect();
    synthesized_adds.sort();
    for path in synthesized_adds {
        if !patch.is_empty() && !patch.ends_with('\n') {
            patch.push('\n');
        }
        patch.push_str(&new_file_hunk(worktree, path)?);
    }

    Ok(CollectedDiff { patch, changes })
}

/// 逐路径授权：覆盖通过返回 Ok；越权与非法路径分别记入拒绝并返回 Err。
fn authorize(scope: &WriteScope, path: &str, rejections: &mut Rejections) -> Result<(), ()> {
    match scope.covers(path) {
        Ok(true) => Ok(()),
        Ok(false) => {
            rejections.unauthorized.push(path.to_owned());
            Err(())
        }
        Err(_) => {
            rejections.invalid_paths.push(path.to_owned());
            Err(())
        }
    }
}

/// 读取 worktree 内文件字节；不存在（删除/竞态）返回 None。
fn fs_bytes(worktree: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(worktree.join(rel)).ok()
}

/// 为新增文件合成 new file hunk（blob 哈希用 `git hash-object --stdin`
/// 计算，不带 -w，不写对象库）。
fn new_file_hunk(worktree: &Path, rel: &str) -> Result<String, DiffError> {
    let bytes = std::fs::read(worktree.join(rel)).map_err(DiffError::from)?;
    let hash = git_stdin(
        worktree,
        &["hash-object", "--stdin"],
        &String::from_utf8_lossy(&bytes),
    )?;
    let text = String::from_utf8(bytes).map_err(|_| {
        DiffError::Git(GitCheckError::UnexpectedOutput {
            detail: format!("新增文件 {rel} 在补丁阶段出现非 UTF-8 内容"),
        })
    })?;
    // 按 \n 手工切分：保留行内的 \r（CRLF）与行尾空格/Tab；末段为空
    // 说明以换行结尾，否则补 "\ No newline at end of file"。
    let segments: Vec<&str> = {
        let stripped = text.strip_suffix('\n').unwrap_or(&text);
        let segments: Vec<&str> = stripped.split('\n').collect();
        segments
    };
    let line_count = segments.len();
    let mut hunk = format!(
        "diff --git a/{rel} b/{rel}\nnew file mode 100644\nindex 0000000..{hash}\n--- /dev/null\n+++ b/{rel}\n"
    );
    if line_count == 1 && segments[0].is_empty() {
        hunk.push_str("@@ -0,0 +0,0 @@\n");
        return Ok(hunk);
    }
    hunk.push_str(&format!("@@ -0,0 +1,{line_count} @@\n"));
    let ends_with_newline = text.ends_with('\n');
    for segment in &segments {
        hunk.push('+');
        hunk.push_str(segment);
        hunk.push('\n');
    }
    if !ends_with_newline {
        hunk.push_str("\\ No newline at end of file\n");
    }
    Ok(hunk)
}
