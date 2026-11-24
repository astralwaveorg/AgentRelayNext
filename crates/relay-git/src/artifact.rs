//! 差异制品的构建与校验（06「产物协议」）。
//!
//! 绑定固定 base OID、attempt、变化清单与原始补丁的 SHA-256；manifest
//! ≤ 64 KiB、补丁 ≤ 512 KiB、变更文件 ≤ 128，超限拒绝、不截断。无变化
//! 或被拒绝的差异不生成可验收制品（被拒差异在采集层即报错，见
//! [`crate::diff`]）。manifest 由输入确定性生成（字段顺序固定），
//! 校验时按同一输入重建并逐字节比较，再独立核对补丁摘要与大小——
//! 补丁改一个字节即校验失败。
//!
//! 发布（06/17）：目标目录、manifest 与收据必须绑定同一 attempt 与
//! 基线；在暂存目录完成写入、逐文件同步与回读校验（含落盘收据的
//! 状态、规则结果与补丁 SHA-256）之后，才以不覆盖目标的原子改名
//! 发布；文件同步失败直接报错，不返回成功。回读按字节上限拒收
//! 超限文件。本包不做真实模型调用或派发。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::diff::{ChangeAction, CollectedDiff};
use crate::receipt::{TestReceipt, MAX_RECEIPT_BYTES};

/// manifest 上限（06：64 KiB）。
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// handoff 上限（06：16 KiB；summary ≤ 2 KiB，不含原始聊天/工具流）。
pub const MAX_HANDOFF_BYTES: usize = 16 * 1024;
/// 补丁上限（06：512 KiB，不截断后冒充可应用补丁）。
pub const MAX_PATCH_BYTES: usize = 512 * 1024;
/// 变更文件数上限（06：最多 128 个变更文件）。
pub const MAX_CHANGED_FILES: usize = 128;

/// 差异制品：manifest（含补丁 SHA-256 与大小）与原始补丁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffArtifact {
    /// 确定性生成的 manifest JSON（schema_version 1）。
    pub manifest_json: String,
    /// 原始补丁（未截断；字节级保真）。
    pub patch: String,
}

/// 制品输入：固定绑定项 + 采集结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactInput<'a> {
    /// attempt opaque ID。
    pub attempt: &'a str,
    /// 固定基线 commit OID。
    pub base_oid: &'a str,
    /// created_at，UTC RFC3339。
    pub created_at: &'a str,
    /// 采集结果（变化清单与补丁）。
    pub collected: &'a CollectedDiff,
}

/// 制品构建与校验错误。
#[derive(Debug)]
pub enum ArtifactError {
    /// 无变化：不生成可验收制品。
    NoChanges,
    /// manifest 超过 64 KiB。
    ManifestTooLarge { size: usize },
    /// 补丁超过 512 KiB。
    PatchTooLarge { size: usize },
    /// 变更文件超过 128 个。
    TooManyFiles { count: usize },
    /// 变化清单非空但补丁为空：清单与补丁必须描述同一份差异。
    EmptyPatch,
    /// manifest 与按输入重建的结果不一致（含被篡改情形）。
    ManifestMismatch,
    /// 补丁 SHA-256 与 manifest 声明不一致（改一个字节即触发）。
    DigestMismatch { expected: String, actual: String },
    /// 补丁大小与 manifest 声明不一致。
    SizeMismatch { expected: usize, actual: usize },
    /// attempt/attempt 标识等字段为空。
    EmptyField { field: &'static str },
    /// 目标目录 attempt 与制品输入（ArtifactInput）attempt 不一致。
    AttemptMismatch { target: String, input: String },
    /// attempt 标识不合法。
    AttemptInvalid {
        attempt: String,
        reason: &'static str,
    },
    /// 同一补丁的收据不是通过状态。
    ReceiptNotPassed { status: String },
    /// 收据摘要与补丁摘要不一致（不同补丁的收据）。
    ReceiptDigestMismatch { expected: String, actual: String },
    /// 收据与 attempt/基线绑定或落盘内容不符（attempt、base_oid、规则
    /// 结果等字段核对失败）。
    ReceiptMismatch { detail: String },
    /// attempt 已发布（不覆盖）。
    AlreadyPublished { path: PathBuf },
    /// 符号链接被拒绝。
    SymlinkRejected { path: PathBuf },
    /// 制品项缺失或不完整。
    MissingArtifactItem { path: PathBuf },
    /// 文件系统操作失败。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 收据超过 64 KiB。
    ReceiptTooLarge { size: usize },
    /// handoff 超过 16 KiB。
    HandoffTooLarge { size: usize },
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactError::NoChanges => write!(f, "无变化：不生成可验收制品"),
            ArtifactError::ManifestTooLarge { size } => {
                write!(f, "manifest {size} 字节超过 64 KiB 上限，拒绝而不截断")
            }
            ArtifactError::PatchTooLarge { size } => {
                write!(f, "补丁 {size} 字节超过 512 KiB 上限，拒绝而不截断")
            }
            ArtifactError::TooManyFiles { count } => {
                write!(f, "变更文件 {count} 个超过 128 个上限，拒绝而不截断")
            }
            ArtifactError::EmptyPatch => {
                write!(f, "变化清单非空但补丁为空：清单与补丁必须描述同一份差异")
            }
            ArtifactError::AttemptInvalid { attempt, reason } => {
                write!(f, "attempt {attempt} 不合法：{reason}")
            }
            ArtifactError::ReceiptNotPassed { status } => {
                write!(f, "收据状态为 {status}：只有同一补丁的通过收据才能发布")
            }
            ArtifactError::ReceiptDigestMismatch { expected, actual } => write!(
                f,
                "收据摘要 {actual} 与补丁摘要 {expected} 不一致（不同补丁的收据）"
            ),
            ArtifactError::ReceiptMismatch { detail } => {
                write!(f, "收据与预期不符：{detail}")
            }
            ArtifactError::AlreadyPublished { path } => {
                write!(f, "attempt 已发布：{}（不覆盖）", path.display())
            }
            ArtifactError::SymlinkRejected { path } => {
                write!(f, "制品路径 {} 是符号链接，拒绝", path.display())
            }
            ArtifactError::MissingArtifactItem { path } => {
                write!(f, "制品项 {} 缺失或不完整", path.display())
            }
            ArtifactError::Io { path, source } => {
                write!(f, "制品文件系统操作失败（{}）：{source}", path.display())
            }
            ArtifactError::ReceiptTooLarge { size } => {
                write!(f, "收据 {size} 字节超过 64 KiB 上限，不截断、不发布")
            }
            ArtifactError::HandoffTooLarge { size } => {
                write!(f, "handoff {size} 字节超过 16 KiB 上限，不截断、不发布")
            }
            ArtifactError::ManifestMismatch => {
                write!(f, "manifest 与按输入重建的结果不一致（疑似被篡改）")
            }
            ArtifactError::DigestMismatch { expected, actual } => write!(
                f,
                "补丁 SHA-256 与 manifest 声明不一致：声明 {expected}，实际 {actual}"
            ),
            ArtifactError::SizeMismatch { expected, actual } => write!(
                f,
                "补丁大小与 manifest 声明不一致：声明 {expected}，实际 {actual}"
            ),
            ArtifactError::EmptyField { field } => {
                write!(f, "制品字段 {field} 为空")
            }
            ArtifactError::AttemptMismatch { target, input } => {
                write!(
                    f,
                    "目标目录 attempt {target} 与制品输入 attempt {input} 不一致"
                )
            }
        }
    }
}

impl std::error::Error for ArtifactError {}

/// SHA-256 的十六进制摘要（小写）。供调用方在发布后核对登记摘要。
pub fn sha256_hex(data: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(data).into();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// JSON 字符串转义（引号、反斜杠与控制字符）。
pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn action_name(action: ChangeAction) -> &'static str {
    match action {
        ChangeAction::Added => "added",
        ChangeAction::Modified => "modified",
        ChangeAction::Deleted => "deleted",
    }
}

/// 构建差异制品。变化清单与补丁来自采集层（被拒差异不会到达这里，
/// 由采集层先行报错）；manifest 按固定字段顺序确定性生成，绑定 base
/// OID、attempt、变化清单与补丁的 SHA-256 及大小。
pub fn build_artifact(input: ArtifactInput<'_>) -> Result<DiffArtifact, ArtifactError> {
    if input.attempt.is_empty() {
        return Err(ArtifactError::EmptyField { field: "attempt" });
    }
    if input.base_oid.is_empty() {
        return Err(ArtifactError::EmptyField { field: "base_oid" });
    }
    if input.created_at.is_empty() {
        return Err(ArtifactError::EmptyField {
            field: "created_at",
        });
    }
    let changes = &input.collected.changes;
    if changes.is_empty() {
        return Err(ArtifactError::NoChanges);
    }
    // 清单与补丁必须描述同一份差异：空补丁配非空清单即拒绝。
    if input.collected.patch.is_empty() {
        return Err(ArtifactError::EmptyPatch);
    }
    if changes.len() > MAX_CHANGED_FILES {
        return Err(ArtifactError::TooManyFiles {
            count: changes.len(),
        });
    }
    let patch_bytes = input.collected.patch.as_bytes();
    if patch_bytes.len() > MAX_PATCH_BYTES {
        return Err(ArtifactError::PatchTooLarge {
            size: patch_bytes.len(),
        });
    }
    let patch_sha = sha256_hex(patch_bytes);
    let files: Vec<String> = changes
        .iter()
        .map(|c| {
            format!(
                "{{\"path\":\"{}\",\"action\":\"{}\"}}",
                json_escape(&c.path),
                action_name(c.action)
            )
        })
        .collect();
    let manifest_json = format!(
        "{{\"schema_version\":1,\"attempt\":\"{}\",\"base_oid\":\"{}\",\"created_at\":\"{}\",\"policy_version\":\"relay-wp2-b3\",\"patch_size_bytes\":{},\"patch_sha256\":\"{}\",\"files\":[{}]}}",
        json_escape(input.attempt),
        json_escape(input.base_oid),
        json_escape(input.created_at),
        patch_bytes.len(),
        patch_sha,
        files.join(",")
    );
    if manifest_json.len() > MAX_MANIFEST_BYTES {
        return Err(ArtifactError::ManifestTooLarge {
            size: manifest_json.len(),
        });
    }
    Ok(DiffArtifact {
        manifest_json,
        patch: input.collected.patch.clone(),
    })
}

/// 从 manifest 提取声明的补丁 SHA-256 与大小（本实现生成的固定格式）。
fn extract_declared(manifest: &str) -> Result<(String, usize), ArtifactError> {
    const SHA_KEY: &str = "\"patch_sha256\":\"";
    const SIZE_KEY: &str = "\"patch_size_bytes\":";
    let sha_start = manifest
        .find(SHA_KEY)
        .map(|i| i + SHA_KEY.len())
        .ok_or(ArtifactError::ManifestMismatch)?;
    let sha_end = manifest[sha_start..]
        .find('"')
        .map(|i| sha_start + i)
        .ok_or(ArtifactError::ManifestMismatch)?;
    let size_start = manifest
        .find(SIZE_KEY)
        .map(|i| i + SIZE_KEY.len())
        .ok_or(ArtifactError::ManifestMismatch)?;
    let size_end = manifest[size_start..]
        .find(',')
        .map(|i| size_start + i)
        .ok_or(ArtifactError::ManifestMismatch)?;
    let size: usize = manifest[size_start..size_end]
        .parse()
        .map_err(|_| ArtifactError::ManifestMismatch)?;
    Ok((manifest[sha_start..sha_end].to_owned(), size))
}

/// 校验差异制品：按同一输入重建 manifest 并逐字节比较，再独立核对
/// 补丁的 SHA-256 与大小——补丁改一个字节即校验失败。
pub fn verify_artifact(
    input: ArtifactInput<'_>,
    artifact: &DiffArtifact,
) -> Result<(), ArtifactError> {
    let rebuilt = build_artifact(input)?;
    if rebuilt.manifest_json != artifact.manifest_json {
        return Err(ArtifactError::ManifestMismatch);
    }
    let (declared_sha, declared_size) = extract_declared(&artifact.manifest_json)?;
    let actual_sha = sha256_hex(artifact.patch.as_bytes());
    if declared_sha != actual_sha {
        return Err(ArtifactError::DigestMismatch {
            expected: declared_sha,
            actual: actual_sha,
        });
    }
    let actual_size = artifact.patch.len();
    if declared_size != actual_size {
        return Err(ArtifactError::SizeMismatch {
            expected: declared_size,
            actual: actual_size,
        });
    }
    Ok(())
}

/// 发布结果：attempt 目录与三个制品文件的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPaths {
    /// attempt 制品目录（受管私有根下，按 attempt 命名）。
    pub dir: PathBuf,
    pub manifest_path: PathBuf,
    pub patch_path: PathBuf,
    pub receipt_path: PathBuf,
}

/// attempt opaque ID 校验：非空、≤128 字节、仅 ASCII 字母数字与 - _，
/// 防路径逃逸（04：opaque、区分大小写）。
fn validate_attempt(attempt: &str) -> Result<(), ArtifactError> {
    let invalid = |reason: &'static str| {
        Err(ArtifactError::AttemptInvalid {
            attempt: attempt.to_owned(),
            reason,
        })
    };
    if attempt.is_empty() || attempt.len() > 128 {
        return invalid("attempt 长度须在 1..=128 字节");
    }
    if !attempt
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return invalid("attempt 只允许 ASCII 字母、数字、- 与 _");
    }
    Ok(())
}

/// 同卷暂存目录名（attempt + 进程/时间熵，防碰撞）。
fn staging_dir(artifact_root: &Path, attempt: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    artifact_root.join(format!(".staging-{attempt}-{}-{nanos}", std::process::id()))
}

/// 发布差异制品（06「产物协议」）。见 [`publish_with_sync`]。
pub fn publish_artifact(
    artifact_root: &Path,
    attempt: &str,
    input: ArtifactInput<'_>,
    receipt: &TestReceipt,
    handoff_json: &str,
) -> Result<PublishPaths, ArtifactError> {
    publish_with_sync(
        artifact_root,
        attempt,
        input,
        receipt,
        handoff_json,
        &|file| file.sync_all(),
    )
}

/// 发布差异制品（06「产物协议」）：
/// 1. 结构检查：制品根为真实目录，attempt 目标不存在——已存在的目标
///    （即使是空目录）一律拒绝，不被替换；
/// 2. 绑定核对：目标目录、ArtifactInput、manifest 与收据的 attempt
///    一致，收据基线与补丁绑定一致，只接受通过收据；
/// 3. 同卷暂存：写入三个文件（create_new）并逐文件同步，暂存目录与
///    父目录 fsync——任何同步失败都直接报错，不报发布成功；
/// 4. 暂存内回读：核对字节、摘要与落盘收据内容（结构化 JSON 解码，
///    拒绝截断与重复关键字段），全部通过才发布；
/// 5. 原子不替换改名到 attempt 目录（平台 RENAME_EXCL：目标已存在——
///    含竞态出现的空目录——原子失败且目标原样保留），父目录同步失败
///    时移除候选并报错；改名后再次回读，不符则移除 attempt 目录。
///
/// 任何失败都只留下暂存垃圾（下次清理），不产生可验收集合。
/// `do_sync` 注入逐文件/目录同步操作，供回归同步失败传播。
fn publish_with_sync(
    artifact_root: &Path,
    attempt: &str,
    input: ArtifactInput<'_>,
    receipt: &TestReceipt,
    handoff_json: &str,
    do_sync: &dyn Fn(&fs::File) -> std::io::Result<()>,
) -> Result<PublishPaths, ArtifactError> {
    validate_attempt(attempt)?;
    // 目标目录 attempt 与制品输入 attempt 必须一致。
    if input.attempt != attempt {
        return Err(ArtifactError::AttemptMismatch {
            target: attempt.to_owned(),
            input: input.attempt.to_owned(),
        });
    }
    // 根目录：真实目录且非符号链接；attempt 目标必须不存在。
    // 结构性检查先于收据校验（链接/覆盖优先报错）。
    let root_meta = fs::symlink_metadata(artifact_root).map_err(|e| ArtifactError::Io {
        path: artifact_root.to_owned(),
        source: e,
    })?;
    if root_meta.is_symlink() {
        return Err(ArtifactError::SymlinkRejected {
            path: artifact_root.to_owned(),
        });
    }
    if !root_meta.is_dir() {
        return Err(ArtifactError::Io {
            path: artifact_root.to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "制品根不是目录"),
        });
    }
    let target = artifact_root.join(attempt);
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_symlink() => {
            return Err(ArtifactError::SymlinkRejected {
                path: target.clone(),
            })
        }
        Ok(_) => {
            return Err(ArtifactError::AlreadyPublished {
                path: target.clone(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ArtifactError::Io {
                path: target.clone(),
                source: e,
            })
        }
    }

    // 收据绑定：目标目录、manifest 与收据指向同一 attempt 与基线。
    if receipt.attempt != attempt {
        return Err(ArtifactError::ReceiptMismatch {
            detail: format!(
                "收据 attempt {} 与发布 attempt {attempt} 不一致",
                receipt.attempt
            ),
        });
    }
    if receipt.base_oid != input.base_oid {
        return Err(ArtifactError::ReceiptMismatch {
            detail: format!(
                "收据基线 {} 与制品基线 {} 不一致",
                receipt.base_oid, input.base_oid
            ),
        });
    }
    // 同一补丁的通过收据。
    if receipt.status != "pass" {
        return Err(ArtifactError::ReceiptNotPassed {
            status: receipt.status.to_owned(),
        });
    }
    let built = build_artifact(input)?;
    let expected_sha = sha256_hex(built.patch.as_bytes());
    if receipt.diff_sha256.as_deref() != Some(expected_sha.as_str()) {
        return Err(ArtifactError::ReceiptDigestMismatch {
            expected: expected_sha,
            actual: receipt.diff_sha256.clone().unwrap_or_default(),
        });
    }
    if receipt.json.len() > MAX_RECEIPT_BYTES {
        return Err(ArtifactError::ReceiptTooLarge {
            size: receipt.json.len(),
        });
    }
    if handoff_json.len() > MAX_HANDOFF_BYTES {
        return Err(ArtifactError::HandoffTooLarge {
            size: handoff_json.len(),
        });
    }

    // 同卷暂存：写入三个文件（create_new），逐文件同步；同步失败即
    // 报错，不得返回成功。
    let staging = staging_dir(artifact_root, attempt);
    fs::create_dir(&staging).map_err(|e| ArtifactError::Io {
        path: staging.clone(),
        source: e,
    })?;
    write_file_with_sync(&staging, "manifest.json", &built.manifest_json, do_sync)?;
    write_file_with_sync(&staging, "diff.patch", &built.patch, do_sync)?;
    write_file_with_sync(&staging, "test_receipt.json", &receipt.json, do_sync)?;
    write_file_with_sync(&staging, "handoff.json", handoff_json, do_sync)?;
    // 暂存目录 fsync（unix 允许对目录 fd sync_all）；失败即报错，
    // 不得报发布成功。
    let dir_file = fs::File::open(&staging).map_err(|e| ArtifactError::Io {
        path: staging.clone(),
        source: e,
    })?;
    do_sync(&dir_file).map_err(|e| ArtifactError::Io {
        path: staging.clone(),
        source: e,
    })?;
    // 暂存内回读：字节、摘要与落盘收据内容全部通过才发布。
    read_back_dir(&staging, attempt, input, receipt, handoff_json)?;
    // 原子不替换改名：暂存 → attempt 目录。平台 renamex_np(RENAME_EXCL)
    // 保证目标已存在（含预检后、改名前竞态出现的空目录）时原子失败
    // 且目标原样保留——不以"再检查一次 + 普通 rename"代替。
    relay_sys::rename_no_replace(&staging, &target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            ArtifactError::AlreadyPublished {
                path: target.clone(),
            }
        } else {
            ArtifactError::Io {
                path: target.clone(),
                source: e,
            }
        }
    })?;
    // 父目录 fsync：失败即报错、移除候选目录并回滚——事务尚未登记，
    // 候选不得通过业务 API 可见，也不留会阻塞重试的孤儿目录。
    let root_file = fs::File::open(artifact_root).map_err(|e| ArtifactError::Io {
        path: artifact_root.to_owned(),
        source: e,
    })?;
    if let Err(err) = do_sync(&root_file) {
        let _ = fs::remove_dir_all(&target);
        return Err(ArtifactError::Io {
            path: artifact_root.to_owned(),
            source: err,
        });
    }

    // 发布后回读核对：不符则移除整个 attempt 目录。
    if let Err(err) = read_back_dir(&target, attempt, input, receipt, handoff_json) {
        let _ = fs::remove_dir_all(&target);
        return Err(err);
    }

    Ok(PublishPaths {
        dir: target.clone(),
        manifest_path: target.join("manifest.json"),
        patch_path: target.join("diff.patch"),
        receipt_path: target.join("test_receipt.json"),
    })
}

/// create_new 写入单文件并执行同步；同步失败原样传播（不得吞掉）。
fn write_file_with_sync(
    dir: &Path,
    name: &str,
    content: &str,
    do_sync: &dyn Fn(&fs::File) -> std::io::Result<()>,
) -> Result<(), ArtifactError> {
    let path = dir.join(name);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|e| ArtifactError::Io {
            path: path.clone(),
            source: e,
        })?;
    use std::io::Write;
    file.write_all(content.as_bytes())
        .map_err(|e| ArtifactError::Io {
            path: path.clone(),
            source: e,
        })?;
    do_sync(&file).map_err(|e| ArtifactError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(())
}

/// 回读并核对制品目录（发布前对暂存目录、发布后对 attempt 目录共用）：
/// 三个文件齐全、非符号链接、大小在上限内、字节与按输入重建的
/// manifest/补丁一致、落盘收据内容（绑定、状态、规则结果、补丁摘要）
/// 与预期一致、补丁 SHA-256 与 manifest 声明一致。
fn read_back_dir(
    dir: &Path,
    attempt: &str,
    input: ArtifactInput<'_>,
    receipt: &TestReceipt,
    expected_handoff: &str,
) -> Result<DiffArtifact, ArtifactError> {
    validate_attempt(attempt)?;
    // 目标目录 attempt 与制品输入 attempt 必须一致。
    if input.attempt != attempt {
        return Err(ArtifactError::AttemptMismatch {
            target: attempt.to_owned(),
            input: input.attempt.to_owned(),
        });
    }
    let meta = fs::symlink_metadata(dir).map_err(|e| ArtifactError::Io {
        path: dir.to_owned(),
        source: e,
    })?;
    if meta.is_symlink() {
        return Err(ArtifactError::SymlinkRejected {
            path: dir.to_owned(),
        });
    }
    if !meta.is_dir() {
        return Err(ArtifactError::MissingArtifactItem {
            path: dir.to_owned(),
        });
    }
    let built = build_artifact(input)?;
    // 回读按可信预期字节上限拒收超限文件：先查元数据快速拒绝，再以
    // 硬上限读取实际字节（不能只看文件元数据——读取时以 take 限定，
    // 超限字节不被读入内存）。
    let read_regular = |name: &str,
                        limit: usize,
                        oversized: fn(usize) -> ArtifactError|
     -> Result<String, ArtifactError> {
        let path = dir.join(name);
        let file_meta = fs::symlink_metadata(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ArtifactError::MissingArtifactItem { path: path.clone() }
            } else {
                ArtifactError::Io {
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
        if file_meta.is_symlink() {
            return Err(ArtifactError::SymlinkRejected { path: path.clone() });
        }
        if !file_meta.is_file() {
            return Err(ArtifactError::MissingArtifactItem { path: path.clone() });
        }
        if file_meta.len() > limit as u64 {
            return Err(oversized(file_meta.len() as usize));
        }
        use std::io::Read;
        let file = fs::File::open(&path).map_err(|e| ArtifactError::Io {
            path: path.clone(),
            source: e,
        })?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ArtifactError::Io {
                path: path.clone(),
                source: e,
            })?;
        if bytes.len() > limit {
            return Err(oversized(bytes.len()));
        }
        String::from_utf8(bytes).map_err(|_| ArtifactError::Io {
            path: path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, "制品文件不是 UTF-8"),
        })
    };
    let manifest_json = read_regular("manifest.json", MAX_MANIFEST_BYTES, |size| {
        ArtifactError::ManifestTooLarge { size }
    })?;
    let patch = read_regular("diff.patch", MAX_PATCH_BYTES, |size| {
        ArtifactError::PatchTooLarge { size }
    })?;
    let receipt_json = read_regular("test_receipt.json", MAX_RECEIPT_BYTES, |size| {
        ArtifactError::ReceiptTooLarge { size }
    })?;
    let handoff_json = read_regular("handoff.json", MAX_HANDOFF_BYTES, |size| {
        ArtifactError::HandoffTooLarge { size }
    })?;

    // 字节级一致（防止落盘后篡改或部分写入）。
    if manifest_json != built.manifest_json {
        return Err(ArtifactError::ManifestMismatch);
    }
    if patch != built.patch {
        return Err(ArtifactError::DigestMismatch {
            expected: sha256_hex(built.patch.as_bytes()),
            actual: sha256_hex(patch.as_bytes()),
        });
    }
    // 落盘收据内容核对（具体错配优先于内存字节对比）。
    verify_receipt_on_disk(
        &receipt_json,
        attempt,
        input.base_oid,
        &sha256_hex(built.patch.as_bytes()),
    )?;
    if receipt_json != receipt.json {
        return Err(ArtifactError::ManifestMismatch);
    }
    if handoff_json != expected_handoff {
        return Err(ArtifactError::ManifestMismatch);
    }
    // 独立核对补丁摘要与大小（与 manifest 声明一致）。
    let (declared_sha, declared_size) = extract_declared(&manifest_json)?;
    let actual_sha = sha256_hex(patch.as_bytes());
    if declared_sha != actual_sha {
        return Err(ArtifactError::DigestMismatch {
            expected: declared_sha,
            actual: actual_sha,
        });
    }
    if declared_size != patch.len() {
        return Err(ArtifactError::SizeMismatch {
            expected: declared_size,
            actual: patch.len(),
        });
    }
    Ok(DiffArtifact {
        manifest_json,
        patch,
    })
}

/// 回读并核对已发布制品：见 [`read_back_dir`]。
pub fn read_back_artifact(
    artifact_root: &Path,
    attempt: &str,
    input: ArtifactInput<'_>,
    receipt: &TestReceipt,
    handoff_json: &str,
) -> Result<DiffArtifact, ArtifactError> {
    read_back_dir(
        &artifact_root.join(attempt),
        attempt,
        input,
        receipt,
        handoff_json,
    )
}

/// 顶层重复关键字段扫描：深度感知地遍历原始文本，只统计顶层对象
/// 内（深度 1）后跟冒号的字符串键；关键字段出现第二次即拒绝。
/// JSON 字符串值内不含未转义引号，按原始字节匹配安全；嵌套对象
/// （rules 条目等）处于更深层级，不计入。
fn scan_top_level_duplicate_keys(text: &str) -> Result<(), String> {
    const KEY_FIELDS: [&str; 6] = [
        "schema_version",
        "attempt",
        "base_oid",
        "diff_sha256",
        "status",
        "rules",
    ];
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0usize;
    let mut counts = [0usize; KEY_FIELDS.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                // 顶层对象内、后跟冒号的字符串是键。
                if depth == 1 {
                    let mut k = i + 1;
                    while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b':' {
                        let key = &text[string_start + 1..i];
                        if let Some(idx) = KEY_FIELDS.iter().position(|f| *f == key) {
                            counts[idx] += 1;
                            if counts[idx] > 1 {
                                return Err(format!("关键字段 {key} 在收据顶层重复"));
                            }
                        }
                    }
                }
            }
        } else {
            match b {
                b'"' => {
                    in_string = true;
                    escaped = false;
                    string_start = i;
                }
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    if depth == 0 {
                        // 顶层对象闭合，扫描结束。
                        break;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    Ok(())
}

/// 核对落盘收据内容：结构化 JSON 解码（serde_json），拒绝截断与
/// 语法错误；原始文本深度感知扫描顶层重复关键字段；绑定同一
/// attempt 与基线、状态为 pass、补丁 SHA-256 一致，且全部规则结果
/// 为 pass（通过收据必须含至少一条规则结果）。落盘收据是验收依据，
/// 不只核对内存字段。
fn verify_receipt_on_disk(
    receipt_json: &str,
    attempt: &str,
    base_oid: &str,
    patch_sha: &str,
) -> Result<(), ArtifactError> {
    let mismatch = |detail: String| ArtifactError::ReceiptMismatch { detail };
    // 1. 结构化解码：截断、语法错误在此暴露。
    let value: serde_json::Value = serde_json::from_str(receipt_json)
        .map_err(|e| mismatch(format!("收据 JSON 解码失败（截断或语法错误）：{e}")))?;
    // 2. 顶层重复关键字段（serde_json Value 不保留重复键，需原文扫描）。
    scan_top_level_duplicate_keys(receipt_json).map_err(mismatch)?;
    // 3. 语义核对。
    let on_attempt = value
        .get("attempt")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| mismatch("收据缺少 attempt 字段".into()))?;
    if on_attempt != attempt {
        return Err(mismatch(format!(
            "落盘收据 attempt {on_attempt} 与发布 attempt {attempt} 不一致"
        )));
    }
    let on_base = value
        .get("base_oid")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| mismatch("收据缺少 base_oid 字段".into()))?;
    if on_base != base_oid {
        return Err(mismatch(format!(
            "落盘收据基线 {on_base} 与制品基线 {base_oid} 不一致"
        )));
    }
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| mismatch("收据缺少 status 字段".into()))?;
    if status != "pass" {
        return Err(ArtifactError::ReceiptNotPassed {
            status: status.to_owned(),
        });
    }
    let sha = value
        .get("diff_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| mismatch("通过收据缺少 diff_sha256（必须绑定补丁摘要）".into()))?;
    if sha != patch_sha {
        return Err(ArtifactError::ReceiptDigestMismatch {
            expected: patch_sha.to_owned(),
            actual: sha.to_owned(),
        });
    }
    // 4. 规则结果：每条 status 都是 pass，且至少一条。
    let rules = value
        .get("rules")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| mismatch("收据缺少 rules 数组".into()))?;
    if rules.is_empty() {
        return Err(mismatch("通过收据不含任何规则结果".into()));
    }
    for entry in rules {
        let rule_id = entry
            .get("rule_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let entry_status = entry
            .get("status")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| mismatch(format!("规则 {rule_id} 的条目缺少 status")))?;
        if entry_status != "pass" {
            return Err(mismatch(format!(
                "规则 {rule_id} 的落盘结果为 {entry_status}，与通过收据不一致"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ChangeEntry;

    /// 同步失败必须传播为错误：不返回成功、不留 attempt 目录。
    #[test]
    fn file_sync_failure_is_propagated_and_publishes_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "relay-git-sync-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let collected = CollectedDiff {
            patch:
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"
                    .to_owned(),
            changes: vec![ChangeEntry {
                path: "a.txt".to_owned(),
                action: ChangeAction::Modified,
            }],
        };
        let input = ArtifactInput {
            attempt: "task_sync-1",
            base_oid: "0123456789abcdef0123456789abcdef01234567",
            created_at: "2026-11-22T01:00:00Z",
            collected: &collected,
        };
        let sha = sha256_hex(collected.patch.as_bytes());
        let receipt = TestReceipt {
            attempt: "task_sync-1".to_owned(),
            base_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            diff_sha256: Some(sha.clone()),
            status: "pass",
            reason: None,
            rules: vec![crate::receipt::RuleReceiptEntry {
                rule_id: "rule_pass".to_owned(),
                display_argv: vec!["/bin/true".to_owned()],
                status: "pass",
                exit_code: Some(0),
                timed_out: false,
                started_at: "2026-11-22T01:00:00Z".to_owned(),
                elapsed_ms: 1,
                output_excerpt: String::new(),
                reason: None,
            }],
            post_rejections: None,
            json: format!(
                "{{\"schema_version\":1,\"attempt\":\"task_sync-1\",\"base_oid\":\"0123456789abcdef0123456789abcdef01234567\",\"diff_sha256\":\"{sha}\",\"post_diff_sha256\":null,\"source_modified\":false,\"status\":\"pass\",\"reason\":null,\"post_rejections\":null,\"rules\":[{{\"rule_id\":\"rule_pass\",\"display_argv\":[\"/bin/true\"],\"status\":\"pass\",\"exit_code\":0,\"timed_out\":false,\"started_at\":\"2026-11-22T01:00:00Z\",\"elapsed_ms\":1,\"output_excerpt\":\"\",\"reason\":null}}]}}"
            ),
        };

        let err = publish_with_sync(&root, "task_sync-1", input, &receipt, "{}", &|_| {
            Err(std::io::Error::other("模拟同步失败"))
        })
        .unwrap_err();
        assert!(
            matches!(err, ArtifactError::Io { .. }),
            "同步失败应作为 Io 错误传播: {err:?}"
        );
        // 不产生可验收集合：attempt 目录不存在。
        assert!(
            !root.join("task_sync-1").exists(),
            "同步失败后不得留下 attempt 目录"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 暂存目录同步失败：第 4 次同步（暂存目录 fd）失败 → 报错且无
    /// attempt 目录。
    #[test]
    fn staging_dir_sync_failure_is_propagated() {
        let dir = std::env::temp_dir().join(format!(
            "relay-git-dirsync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let collected = CollectedDiff {
            patch:
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"
                    .to_owned(),
            changes: vec![ChangeEntry {
                path: "a.txt".to_owned(),
                action: ChangeAction::Modified,
            }],
        };
        let sha = sha256_hex(collected.patch.as_bytes());
        let receipt = TestReceipt {
            attempt: "task_sync-1".to_owned(),
            base_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            diff_sha256: Some(sha.clone()),
            status: "pass",
            reason: None,
            rules: vec![crate::receipt::RuleReceiptEntry {
                rule_id: "rule_pass".to_owned(),
                display_argv: vec!["/bin/true".to_owned()],
                status: "pass",
                exit_code: Some(0),
                timed_out: false,
                started_at: "2026-11-22T01:00:00Z".to_owned(),
                elapsed_ms: 1,
                output_excerpt: String::new(),
                reason: None,
            }],
            post_rejections: None,
            json: format!(
                "{{\"schema_version\":1,\"attempt\":\"task_sync-1\",\"base_oid\":\"0123456789abcdef0123456789abcdef01234567\",\"diff_sha256\":\"{sha}\",\"post_diff_sha256\":null,\"source_modified\":false,\"status\":\"pass\",\"reason\":null,\"post_rejections\":null,\"rules\":[{{\"rule_id\":\"rule_pass\",\"display_argv\":[\"/bin/true\"],\"status\":\"pass\",\"exit_code\":0,\"timed_out\":false,\"started_at\":\"2026-11-22T01:00:00Z\",\"elapsed_ms\":1,\"output_excerpt\":\"\",\"reason\":null}}]}}"
            ),
        };
        let input = ArtifactInput {
            attempt: "task_sync-1",
            base_oid: "0123456789abcdef0123456789abcdef01234567",
            created_at: "2026-11-22T01:00:00Z",
            collected: &collected,
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = publish_with_sync(&root, "task_sync-1", input, &receipt, "{}", &|_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // 前 3 次 = 三个制品文件；第 4 次 = 暂存目录。
            if n >= 3 {
                Err(std::io::Error::other("模拟暂存目录同步失败"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(err, ArtifactError::Io { .. }), "{err:?}");
        assert!(
            !root.join("task_sync-1").exists(),
            "暂存目录同步失败不得发布"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 父目录同步失败：第 5 次同步失败 → 不报发布成功（改名已发生，
    /// 内容已核对，重试得到 AlreadyPublished）。
    #[test]
    fn parent_dir_sync_failure_is_propagated() {
        let dir = std::env::temp_dir().join(format!(
            "relay-git-parentsync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let collected = CollectedDiff {
            patch:
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"
                    .to_owned(),
            changes: vec![ChangeEntry {
                path: "a.txt".to_owned(),
                action: ChangeAction::Modified,
            }],
        };
        let sha = sha256_hex(collected.patch.as_bytes());
        let receipt = TestReceipt {
            attempt: "task_sync-1".to_owned(),
            base_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            diff_sha256: Some(sha.clone()),
            status: "pass",
            reason: None,
            rules: vec![crate::receipt::RuleReceiptEntry {
                rule_id: "rule_pass".to_owned(),
                display_argv: vec!["/bin/true".to_owned()],
                status: "pass",
                exit_code: Some(0),
                timed_out: false,
                started_at: "2026-11-22T01:00:00Z".to_owned(),
                elapsed_ms: 1,
                output_excerpt: String::new(),
                reason: None,
            }],
            post_rejections: None,
            json: format!(
                "{{\"schema_version\":1,\"attempt\":\"task_sync-1\",\"base_oid\":\"0123456789abcdef0123456789abcdef01234567\",\"diff_sha256\":\"{sha}\",\"post_diff_sha256\":null,\"source_modified\":false,\"status\":\"pass\",\"reason\":null,\"post_rejections\":null,\"rules\":[{{\"rule_id\":\"rule_pass\",\"display_argv\":[\"/bin/true\"],\"status\":\"pass\",\"exit_code\":0,\"timed_out\":false,\"started_at\":\"2026-11-22T01:00:00Z\",\"elapsed_ms\":1,\"output_excerpt\":\"\",\"reason\":null}}]}}"
            ),
        };
        let input = ArtifactInput {
            attempt: "task_sync-1",
            base_oid: "0123456789abcdef0123456789abcdef01234567",
            created_at: "2026-11-22T01:00:00Z",
            collected: &collected,
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let err = publish_with_sync(&root, "task_sync-1", input, &receipt, "{}", &|_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // 前 4 次成功（3 文件 + 暂存目录）；第 5 次 = 父目录。
            if n >= 4 {
                Err(std::io::Error::other("模拟父目录同步失败"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(err, ArtifactError::Io { .. }), "{err:?}");
        // 事务尚未登记：候选目录随父目录同步失败回滚，业务 API 不可见，
        // 也不留阻塞重试的孤儿目录。
        assert!(
            !root.join("task_sync-1").exists(),
            "父目录同步失败后候选必须回滚移除"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 已独立复现的回归（17：f22c0b1）：预检后、改名前出现的空目标
    /// 目录曾被覆盖。在暂存目录同步注入点（第 4 次同步）创建空目标
    /// 目录后继续发布，必须得到 AlreadyPublished 且原目录保持为空。
    #[test]
    fn empty_target_appearing_at_sync_seam_is_not_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "relay-git-racerename-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let root = dir.join("artifacts");
        fs::create_dir_all(&root).unwrap();
        let collected = CollectedDiff {
            patch:
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"
                    .to_owned(),
            changes: vec![ChangeEntry {
                path: "a.txt".to_owned(),
                action: ChangeAction::Modified,
            }],
        };
        let sha = sha256_hex(collected.patch.as_bytes());
        let receipt_json = format!(
            "{{\"schema_version\":1,\"attempt\":\"task_sync-1\",\"base_oid\":\"0123456789abcdef0123456789abcdef01234567\",\"diff_sha256\":\"{sha}\",\"post_diff_sha256\":null,\"source_modified\":false,\"status\":\"pass\",\"reason\":null,\"post_rejections\":null,\"rules\":[{{\"rule_id\":\"rule_pass\",\"display_argv\":[\"/bin/true\"],\"status\":\"pass\",\"exit_code\":0,\"timed_out\":false,\"started_at\":\"2026-11-22T01:00:00Z\",\"elapsed_ms\":1,\"output_excerpt\":\"\",\"reason\":null}}]}}"
        );
        let receipt = TestReceipt {
            attempt: "task_sync-1".to_owned(),
            base_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            diff_sha256: Some(sha),
            status: "pass",
            reason: None,
            rules: vec![crate::receipt::RuleReceiptEntry {
                rule_id: "rule_pass".to_owned(),
                display_argv: vec!["/bin/true".to_owned()],
                status: "pass",
                exit_code: Some(0),
                timed_out: false,
                started_at: "2026-11-22T01:00:00Z".to_owned(),
                elapsed_ms: 1,
                output_excerpt: String::new(),
                reason: None,
            }],
            post_rejections: None,
            json: receipt_json,
        };
        let input = ArtifactInput {
            attempt: "task_sync-1",
            base_oid: "0123456789abcdef0123456789abcdef01234567",
            created_at: "2026-11-22T01:00:00Z",
            collected: &collected,
        };
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let target = root.join("task_sync-1");
        let target_probe = target.clone();
        let err = publish_with_sync(&root, "task_sync-1", input, &receipt, "{}", &|_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 3 {
                // 暂存目录同步（第 4 次）：此刻竞态插入空目标目录。
                std::fs::create_dir_all(&target_probe).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            matches!(err, ArtifactError::AlreadyPublished { .. }),
            "空目标目录竞态应原子拒绝: {err:?}"
        );
        // 原目录保持为空：未被移走、未被填充。
        let entries: Vec<_> = fs::read_dir(&target).unwrap().collect();
        assert!(entries.is_empty(), "空目录不得被覆盖: {entries:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
