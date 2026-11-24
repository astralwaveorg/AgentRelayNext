//! 只读 Git 基线预检（06「工作副本生命周期」§1、04 OID 约束）。
//!
//! 仅接受登记的仓库根目录和该仓库对象格式对应的完整小写 commit OID。
//! Git 一律结构化 argv 调用（不经 shell、不做字符串拼接），并显式移除
//! 可能改变仓库定位的环境变量、关闭可选锁与终端提示；对象读取通过
//! `protocol.allow=never` 明确禁用自动联网补对象。本包调用的子命令只读
//! 对象库与 refs：不执行 hooks、不触发 clean/smudge 过滤器、不创建
//! worktree、不写 index。同时预检固定基线提交树：含 Gitlink（mode
//! 160000）即拒绝，即使 submodule 未初始化且本地没有 submodule 配置。
//! 对 partial clone、自定义过滤器、replace 引用和非常规对象格式等
//! 不能安全确认的仓库类型一律保守拒绝。

use std::fmt;
use std::path::{Path, PathBuf};

use crate::git_layer::{git_out, git_query, git_raw};

/// 基线预检结果：已确认的 commit OID 与仓库对象格式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineReport {
    /// 与调用方输入一致的完整小写 commit OID。
    pub commit_oid: String,
    /// 仓库对象格式：`sha1` 或 `sha256`。
    pub object_format: &'static str,
}

/// 基线预检错误。错误信息只含命令 argv、截断后的 stderr 与路径，不含
/// 仓库内容原文。
#[derive(Debug)]
pub enum GitCheckError {
    /// OID 文本不符合外部约束：非小写十六进制（分支名、大写）或长度
    /// 不是 40/64（缩写、其他格式长度）。
    InvalidOid { oid: String, reason: &'static str },
    /// 目录不是可确认的 Git 工作树根（非仓库、裸仓库、无法运行 git）。
    NotARepository { root: PathBuf },
    /// 仓库 toplevel 与登记根目录不一致（子目录或其他仓库）。
    RootMismatch { root: PathBuf, toplevel: PathBuf },
    /// 对象格式不被本实现支持。
    UnsupportedObjectFormat { format: String },
    /// 对象不存在（含 partial clone 下无法本地确认的对象）。
    ObjectMissing { oid: String },
    /// 对象存在但不是 commit。
    NotACommit { oid: String, kind: String },
    /// 仓库类型包含本版无法安全确认的能力（partial clone、submodule、
    /// 自定义过滤器、replace 引用）。
    UnsafeRepository { reason: String },
    /// Git 子命令失败（argv 与截断 stderr）。
    CommandFailed { argv: String, stderr: String },
    /// Git 输出不符合预期格式。
    UnexpectedOutput { detail: String },
    /// 无法启动 git 或路径解析失败。
    GitInvocation(String),
}

impl fmt::Display for GitCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitCheckError::InvalidOid { oid, reason } => {
                write!(f, "commit OID {oid} 不合法：{reason}")
            }
            GitCheckError::NotARepository { root } => {
                write!(f, "{} 不是可确认的 Git 工作树根", root.display())
            }
            GitCheckError::RootMismatch { root, toplevel } => write!(
                f,
                "登记根目录 {} 与仓库 toplevel {} 不一致",
                root.display(),
                toplevel.display()
            ),
            GitCheckError::UnsupportedObjectFormat { format } => {
                write!(f, "仓库对象格式 {format} 不被支持，拒绝预检")
            }
            GitCheckError::ObjectMissing { oid } => {
                write!(f, "对象 {oid} 在本地对象库中不存在（不联网补对象）")
            }
            GitCheckError::NotACommit { oid, kind } => {
                write!(f, "对象 {oid} 是 {kind}，不是 commit")
            }
            GitCheckError::UnsafeRepository { reason } => {
                write!(f, "仓库含本版无法安全确认的能力，拒绝预检：{reason}")
            }
            GitCheckError::CommandFailed { argv, stderr } => {
                write!(f, "git 命令失败（{argv}）：{stderr}")
            }
            GitCheckError::UnexpectedOutput { detail } => {
                write!(f, "Git 输出异常：{detail}")
            }
            GitCheckError::GitInvocation(message) => {
                write!(f, "git 调用失败：{message}")
            }
        }
    }
}

impl std::error::Error for GitCheckError {}

/// OID 文本预检：全小写十六进制，长度 40 或 64（具体与仓库对象格式的
/// 匹配在读取格式后核对）。分支名、缩写与大写在文本层即被拒绝。
fn validate_oid_text(oid: &str) -> Result<(), GitCheckError> {
    let invalid = |reason: &'static str| {
        Err(GitCheckError::InvalidOid {
            oid: oid.to_owned(),
            reason,
        })
    };
    if oid.is_empty() {
        return invalid("为空");
    }
    if !oid
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return invalid("须为完整小写十六进制 commit OID，不接受分支名、缩写或大写");
    }
    if oid.len() != 40 && oid.len() != 64 {
        return invalid("长度必须是 40（SHA-1）或 64（SHA-256）个字符");
    }
    Ok(())
}

/// 只读基线预检：确认登记根目录是仓库工作树根、对象格式受支持、
/// OID 与格式匹配且对象存在且为 commit，并保守拒绝本版无法安全确认的
/// 仓库类型。全程只读：不改 HEAD、index 或工作文件。
pub fn read_repository_head(repo_root: &Path) -> Result<String, GitCheckError> {
    let oid = git_out(repo_root, &["rev-parse", "--verify", "HEAD"])?;
    validate_oid_text(&oid)?;
    Ok(oid)
}

pub fn verify_baseline(
    repo_root: &Path,
    commit_oid: &str,
) -> Result<BaselineReport, GitCheckError> {
    validate_oid_text(commit_oid)?;
    if !repo_root.is_dir() {
        return Err(GitCheckError::NotARepository {
            root: repo_root.to_owned(),
        });
    }
    // 1. 仓库身份：toplevel 必须与登记根一致（符号链接归一后比较）。
    let (ok, _code, stdout, stderr) = git_raw(repo_root, &["rev-parse", "--show-toplevel"])?;
    if !ok {
        return Err(GitCheckError::NotARepository {
            root: repo_root.to_owned(),
        });
    }
    let _ = stderr;
    let toplevel = stdout.trim_end().to_owned();
    let root_canonical = std::fs::canonicalize(repo_root)
        .map_err(|e| GitCheckError::GitInvocation(format!("无法解析登记根目录：{e}")))?;
    let toplevel_canonical = std::fs::canonicalize(Path::new(&toplevel))
        .map_err(|e| GitCheckError::GitInvocation(format!("无法解析仓库 toplevel：{e}")))?;
    if root_canonical != toplevel_canonical {
        return Err(GitCheckError::RootMismatch {
            root: root_canonical,
            toplevel: toplevel_canonical,
        });
    }
    // 2. 对象格式决定 OID 长度。
    let format = git_out(repo_root, &["rev-parse", "--show-object-format"])?;
    let expected_len = match format.as_str() {
        "sha1" => 40,
        "sha256" => 64,
        other => {
            return Err(GitCheckError::UnsupportedObjectFormat {
                format: other.to_owned(),
            })
        }
    };
    if commit_oid.len() != expected_len {
        return Err(GitCheckError::InvalidOid {
            oid: commit_oid.to_owned(),
            reason: "长度与本仓库对象格式不符",
        });
    }
    // 3. 保守拒绝本版无法安全确认的仓库能力。
    if git_query(
        repo_root,
        &["config", "--local", "--get", "extensions.partialclone"],
    )?
    .is_some()
    {
        return Err(GitCheckError::UnsafeRepository {
            reason: "partial clone（对象可能缺失且依赖联网补齐）".into(),
        });
    }
    if git_query(
        repo_root,
        &["config", "--local", "--get-regexp", r"^submodule\."],
    )?
    .is_some()
    {
        return Err(GitCheckError::UnsafeRepository {
            reason: "配置了 submodule".into(),
        });
    }
    if git_query(
        repo_root,
        &["config", "--local", "--get-regexp", r"^filter\."],
    )?
    .is_some()
    {
        return Err(GitCheckError::UnsafeRepository {
            reason: "配置了自定义过滤器（如 LFS）".into(),
        });
    }
    let replace_refs = git_query(
        repo_root,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "refs/replace/",
        ],
    )?;
    if replace_refs
        .as_deref()
        .unwrap_or("")
        .contains("refs/replace/")
    {
        return Err(GitCheckError::UnsafeRepository {
            reason: "存在 refs/replace 替换引用".into(),
        });
    }
    // 4. 对象存在且为 commit（cat-file -t 只读对象库，不触发联网补对象）。
    let (ok, code, stdout, stderr) = git_raw(repo_root, &["cat-file", "-t", commit_oid])?;
    if !ok {
        return Err(GitCheckError::ObjectMissing {
            oid: commit_oid.to_owned(),
        });
    }
    let _ = code;
    let kind = stdout.trim_end().to_owned();
    if kind != "commit" {
        return Err(GitCheckError::NotACommit {
            oid: commit_oid.to_owned(),
            kind,
        });
    }
    // 5. 固定基线提交树不得含 Gitlink（mode 160000）条目：子模块在本版
    //    未经安全确认，且其对象存在与否不依赖本地 submodule 配置或初始化。
    let tree_listing = git_out(repo_root, &["ls-tree", "-r", commit_oid])?;
    if tree_listing
        .lines()
        .any(|line| line.split_whitespace().next() == Some("160000"))
    {
        return Err(GitCheckError::UnsafeRepository {
            reason: "基线提交树含 Gitlink（mode 160000）条目，子模块未在本版安全确认范围内".into(),
        });
    }
    let _ = stderr;
    let object_format = match format.as_str() {
        "sha1" => "sha1",
        _ => "sha256",
    };
    Ok(BaselineReport {
        commit_oid: commit_oid.to_owned(),
        object_format,
    })
}
