//! Agent Relay WP2-A/B：Git 层的纯路径规则、只读预检与受管 worktree。
//!
//! - [`scope`]：写入范围（write_scope）的路径校验、匹配与两层授权交集，
//!   依据 06「工作副本生命周期」§3 与 04 字段约束。
//! - [`baseline`]：只读 Git 基线预检（仓库根身份、对象格式、commit 对象），
//!   依据 06 §1 与 04 的 OID 约束。
//! - [`worktree`]：固定基线的受管 detached worktree 创建（06 §2），
//!   目标由受管根与 task/attempt 标识生成。
//! - [`diff`]：受管 worktree 相对固定基线的真实变化采集与授权检查
//!   （06 §4），合规文本变化生成固定顺序补丁。
//! - [`artifact`]：差异制品的构建与校验（06「产物协议」）——绑定
//!   base OID/attempt/变化清单/补丁 SHA-256，64 KiB、512 KiB、128 文件
//!   上限，无变化不生成制品。
//! - [`verify`]：受控验证执行（08 T10）——任务只传已批准的规则 ID，
//!   可执行文件与 argv 从可信配置解析，不经 shell、不继承环境。
//! - [`receipt`]：测试收据（06 test_receipt）——绑定 attempt/规则 ID/
//!   被测补丁 SHA-256/实际结果，64 KiB 上限，摘要每条 ≤ 2 KiB。
//!
//! Git 调用统一经 [`git_layer`]（crate 内共享）：环境加固含空
//! `GIT_ALLOW_PROTOCOL`、`GIT_NO_LAZY_FETCH=1` 与 `-c
//! protocol.allow=never`，传输协议不能被单协议配置放开。
//!
//! 本 crate 做规则、预检、受管创建、差异采集、制品构建、受控验证与
//! 测试收据：不做制品落盘发布、清理、进程组回收或真实模型调用，也
//! 不声称已解决文件系统链接检查。Git 一律结构化 argv 调用，不经 shell。

#![forbid(unsafe_code)]

pub mod artifact;
pub mod baseline;
pub mod diff;
mod git_layer;
pub mod receipt;
pub mod scope;
pub mod verify;
pub mod worktree;

pub use artifact::{
    build_artifact, publish_artifact, read_back_artifact, sha256_hex, verify_artifact,
    ArtifactError, ArtifactInput, DiffArtifact, PublishPaths, MAX_CHANGED_FILES,
    MAX_MANIFEST_BYTES, MAX_PATCH_BYTES,
};
pub use baseline::{read_repository_head, verify_baseline, BaselineReport, GitCheckError};
pub use diff::{collect_diff, ChangeAction, ChangeEntry, CollectedDiff, DiffError, Rejections};
pub use receipt::{
    run_and_build_receipt, ReceiptError, RuleReceiptEntry, TestReceipt, MAX_RECEIPT_BYTES,
};
pub use scope::{
    validate_entry, ScopeError, ScopeRule, ScopeRuleKind, WriteScope, MAX_SCOPE_RULES,
    MAX_SCOPE_RULE_BYTES,
};
pub use verify::{
    run_rule, VerificationOutcome, VerificationStatus, VerifyConfig, VerifyError, VerifyRule,
    MAX_OUTPUT_EXCERPT_BYTES,
};
pub use worktree::{create_worktree, WorktreeError, WorktreeReport};
