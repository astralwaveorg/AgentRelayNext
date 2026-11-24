//! 受管 detached worktree（06「工作副本生命周期」§2）。
//!
//! 目标路径由受管根与 task/attempt 标识生成，不接收调用方任意路径：
//! 拒绝已存在、越界逃逸或含符号链接组件的目标，不复用、不覆盖。创建前
//! 必须通过 [`verify_baseline`]（预检失败就不创建）；创建经共享 Git 调用
//! 层执行 `git worktree add --detach`，hooks 经 `core.hooksPath=/dev/null`
//! 显式禁用，过滤器与 partial clone 仓库在预检即拒绝，传输协议已禁用，
//! 不存在隐式联网。主 checkout 的分支、文件与 index 不变；对象库与 refs
//! 共用。
//!
//! 本模块只创建：持久记录 worktree 身份、自动清理、diff 与派发属后续
//! 任务包，由调用方在创建成功后落库（WP1）。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::baseline::verify_baseline;
use crate::git_layer::git_out;

/// 创建成功的受管 worktree 信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeReport {
    /// 实际创建的 worktree 路径（受管根下按 task/attempt 生成）。
    pub path: PathBuf,
    /// 固定基线 commit OID（与预检输入一致）。
    pub base_oid: String,
    /// worktree 内 HEAD（detached，等于 base_oid）。
    pub head_oid: String,
    /// 仓库对象格式。
    pub object_format: &'static str,
}

/// 受管 worktree 请求参数不合法（task_id 形状、attempt 编号）。
#[derive(Debug)]
pub enum WorktreeError {
    /// 参数不合法。
    InvalidRequest { reason: String },
    /// 目标路径不可用（已存在、含符号链接组件等；不复用、不覆盖）。
    TargetInvalid { path: PathBuf, reason: String },
    /// 基线预检或 Git 调用失败。
    Check(crate::GitCheckError),
    /// 创建后校验失败（HEAD 与基线不一致、未处于 detached 状态等）。
    PostCreateCheck { reason: String },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::InvalidRequest { reason } => {
                write!(f, "受管 worktree 请求不合法：{reason}")
            }
            WorktreeError::TargetInvalid { path, reason } => write!(
                f,
                "worktree 目标 {} 不可用（{}），不复用、不覆盖",
                path.display(),
                reason
            ),
            WorktreeError::Check(e) => write!(f, "基线预检或 Git 调用失败：{e}"),
            WorktreeError::PostCreateCheck { reason } => {
                write!(f, "worktree 创建后校验失败：{reason}")
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

impl From<crate::GitCheckError> for WorktreeError {
    fn from(e: crate::GitCheckError) -> Self {
        WorktreeError::Check(e)
    }
}

/// 待校验的受管路径检查：managed_root 及其下各组件只做元数据检查，
/// 不创建目录、不解析符号链接。符号链接、类型不符或目标已存在即拒绝。
fn reject_invalid_managed_path(
    managed_root: &Path,
    parts: &[PathBuf],
) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(managed_root) {
        Ok(meta) => {
            if meta.is_symlink() {
                return Err(WorktreeError::TargetInvalid {
                    path: managed_root.to_owned(),
                    reason: "受管根是符号链接".into(),
                });
            }
            if !meta.is_dir() {
                return Err(WorktreeError::TargetInvalid {
                    path: managed_root.to_owned(),
                    reason: "受管根不是目录".into(),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(WorktreeError::TargetInvalid {
                path: managed_root.to_owned(),
                reason: format!("状态检查失败：{e}"),
            })
        }
    }
    let mut prefix = managed_root.to_owned();
    for (index, part) in parts.iter().enumerate() {
        prefix = prefix.join(part);
        let is_last = index + 1 == parts.len();
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => {
                if meta.is_symlink() {
                    return Err(WorktreeError::TargetInvalid {
                        path: prefix.clone(),
                        reason: "受管路径组件是符号链接".into(),
                    });
                }
                if !meta.is_dir() {
                    return Err(WorktreeError::TargetInvalid {
                        path: prefix.clone(),
                        reason: "受管路径组件不是目录".into(),
                    });
                }
                if is_last {
                    return Err(WorktreeError::TargetInvalid {
                        path: prefix.clone(),
                        reason: "目标已存在".into(),
                    });
                }
            }
            // 更深处不可能存在，停止下探。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(WorktreeError::TargetInvalid {
                    path: prefix.clone(),
                    reason: format!("状态检查失败：{e}"),
                })
            }
        }
    }
    Ok(())
}

/// 解析受管路径中最近存在的祖先（只读；受管路径上的符号链接已在
/// 前置检查中拒绝），再拼回尚不存在的剩余词法组件。
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, WorktreeError> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur: &Path = path;
    loop {
        match cur.symlink_metadata() {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(WorktreeError::TargetInvalid {
                    path: cur.to_owned(),
                    reason: format!("状态检查失败：{e}"),
                })
            }
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) if parent != cur => {
                missing.push(name.to_owned());
                cur = parent;
            }
            _ => break,
        }
    }
    let resolved = fs::canonicalize(cur).map_err(|e| WorktreeError::TargetInvalid {
        path: cur.to_owned(),
        reason: format!("无法解析既存路径：{e}"),
    })?;
    let mut resolved_path = resolved;
    for part in missing.iter().rev() {
        resolved_path.push(part);
    }
    Ok(resolved_path)
}

/// task/attempt 标识的路径安全约束：非空、≤128 字节、仅 ASCII 字母、
/// 数字、`-` 与 `_`，不能作为路径组件逃逸受管根。
fn validate_task_id(task_id: &str) -> Result<(), WorktreeError> {
    let invalid = |reason: &'static str| {
        Err(WorktreeError::InvalidRequest {
            reason: reason.to_owned(),
        })
    };
    if task_id.is_empty() || task_id.len() > 128 {
        return invalid("task_id 长度须在 1..=128 字节");
    }
    if !task_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return invalid("task_id 只允许 ASCII 字母、数字、- 与 _");
    }
    Ok(())
}

/// 创建固定基线的受管 detached worktree。
///
/// 目标路径 = `<受管根>/worktrees/<task_id>/attempt-<n>`，由受管根与
/// 标识生成；已存在、越界（task_id 逃逸已被字符集排除）或含符号链接
/// 组件的目标直接拒绝，不复用、不覆盖。创建顺序：参数校验 → 基线预检
/// （失败就不创建）→ 目标检查 → `git worktree add --detach`（hooks 经
/// `core.hooksPath=/dev/null` 显式禁用；过滤器与 partial clone 仓库在
/// 预检即拒绝；传输协议已禁用）→ 校验 worktree 内 HEAD 为基线且处于
/// detached 状态。
pub fn create_worktree(
    repo_root: &Path,
    managed_root: &Path,
    task_id: &str,
    attempt_no: u32,
    base_oid: &str,
) -> Result<WorktreeReport, WorktreeError> {
    validate_task_id(task_id)?;
    if attempt_no == 0 {
        return Err(WorktreeError::InvalidRequest {
            reason: "attempt 编号必须为正整数".into(),
        });
    }
    // 预检失败就不创建：仓库身份、对象格式、Gitlink/过滤器/partial
    // clone 等在此一并把关。
    let report = verify_baseline(repo_root, base_oid)?;

    // 已解析的可信父路径：主 checkout 根（预检已确认是工作树根）。
    let repo_canonical = fs::canonicalize(repo_root).map_err(|e| WorktreeError::TargetInvalid {
        path: repo_root.to_owned(),
        reason: format!("无法解析主 checkout 根：{e}"),
    })?;
    let parts = [
        PathBuf::from("worktrees"),
        PathBuf::from(task_id),
        PathBuf::from(format!("attempt-{attempt_no}")),
    ];
    // 待校验的受管路径：只做元数据检查，不创建目录、不解析符号链接。
    // managed_root 及其下任一组件是符号链接、类型不符或目标已存在时拒绝。
    reject_invalid_managed_path(managed_root, &parts)?;
    // 受管根不得位于主 checkout 内：解析最近存在的祖先（只读，受管路径
    // 上的符号链接已在此前拒绝），拼回尚不存在的剩余词法组件后做组件级
    // 包含比较。
    let resolved_managed = resolve_existing_prefix(managed_root)?;
    if resolved_managed == repo_canonical || resolved_managed.starts_with(&repo_canonical) {
        return Err(WorktreeError::TargetInvalid {
            path: managed_root.to_owned(),
            reason: "受管根位于主 checkout 内".into(),
        });
    }
    // 校验全部通过才开始创建目录。
    fs::create_dir_all(managed_root).map_err(|e| WorktreeError::TargetInvalid {
        path: managed_root.to_owned(),
        reason: format!("无法创建受管根目录：{e}"),
    })?;
    let managed_canonical =
        fs::canonicalize(managed_root).map_err(|e| WorktreeError::TargetInvalid {
            path: managed_root.to_owned(),
            reason: format!("无法解析受管根目录：{e}"),
        })?;
    let target = managed_canonical
        .join("worktrees")
        .join(task_id)
        .join(format!("attempt-{attempt_no}"));
    // 创建前按同一实现再走一遍逐级检查（含“目标已存在”拒绝）。
    reject_invalid_managed_path(&managed_canonical, &parts)?;
    // hooks 显式禁用：core.hooksPath=/dev/null 使 post-checkout 等
    // 钩子无处执行；传输协议与懒取已在共享层关闭。
    git_out(
        repo_root,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "--detach",
            target
                .to_str()
                .ok_or_else(|| WorktreeError::InvalidRequest {
                    reason: "目标路径不是合法 UTF-8".into(),
                })?,
            base_oid,
        ],
    )?;
    // 校验：worktree 存在、HEAD 为固定基线且 detached。
    let worktree_head = git_out(&target, &["rev-parse", "HEAD"])?;
    if worktree_head != base_oid {
        return Err(WorktreeError::PostCreateCheck {
            reason: format!("worktree HEAD {worktree_head} 与基线 {base_oid} 不一致"),
        });
    }
    let branch_ref = git_out(&target, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch_ref != "HEAD" {
        return Err(WorktreeError::PostCreateCheck {
            reason: format!("worktree 不是 detached 状态（指向 {branch_ref}）"),
        });
    }
    Ok(WorktreeReport {
        path: target,
        base_oid: base_oid.to_owned(),
        head_oid: worktree_head,
        object_format: report.object_format,
    })
}
