//! 测试收据（06「产物协议」test_receipt、08 T10）：绑定 attempt、规则
//! ID、被测补丁的 SHA-256 与实际运行结果。
//!
//! 验证前后各做一次与固定基线的差异采集并核对摘要：验证前与被测补丁
//! 不一致（差异漂移）、验证改了源码、或任何规则失败/超时/未启动，都
//! 不得出具通过收据——收据照常生成，状态为 failed 并附稳定原因。
//! 只保留受限诊断（每条输出摘要 ≤ 2 KiB，来自受控验证层），原始
//! stdout/stderr 不持久化；收据整体 ≤ 64 KiB，超限报错不截断。
//! 落盘发布与真实模型调用属后续任务包。

use std::fmt;
use std::path::Path;

use crate::artifact::{json_escape, sha256_hex};
use crate::diff::{collect_diff, CollectedDiff, DiffError};
use crate::scope::WriteScope;
use crate::verify::{run_rule, VerifyConfig, VerifyError, VerifyRule};

/// 收据上限（06：test_receipt 64 KiB）。
pub const MAX_RECEIPT_BYTES: usize = 64 * 1024;

/// 单条规则的收据条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleReceiptEntry {
    pub rule_id: String,
    /// 显示用安全 argv（可执行文件 + 固定参数）。
    pub display_argv: Vec<String>,
    /// `pass` / `fail` / `timeout` / `not_run`（06 允许的状态集）。
    pub status: &'static str,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub started_at: String,
    pub elapsed_ms: u64,
    /// 受限输出摘要（≤ 2 KiB）。
    pub output_excerpt: String,
    /// not_run 或失败原因（稳定文本）。
    pub reason: Option<String>,
}

/// 测试收据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReceipt {
    pub attempt: String,
    pub base_oid: String,
    /// 被验证的实际差异的 SHA-256；预检被拒（未采集）时为 None，
    /// 不填空补丁的哈希。
    pub diff_sha256: Option<String>,
    /// `pass` / `failed`：任何规则失败、超时、未启动或差异漂移均为 failed。
    pub status: &'static str,
    /// failed 时的稳定原因。
    pub reason: Option<String>,
    pub rules: Vec<RuleReceiptEntry>,
    /// 验证后被 Git 层拒绝的改动证据（只含路径，不含文件内容）。
    pub post_rejections: Option<crate::diff::Rejections>,
    /// 确定性 JSON 序列化（schema_version 1）。
    pub json: String,
}

/// 收据错误（硬性失败：收据无法生成）。
#[derive(Debug)]
pub enum ReceiptError {
    /// 未提供任何规则：空规则集不允许出具通过收据。
    NoRules,
    /// 差异采集失败（Git 层）。
    Collect(crate::diff::DiffError),
    /// 规则配置或 ID 问题。
    Verify(VerifyError),
    /// 收据超过 64 KiB（不截断，不发布）。
    ReceiptTooLarge { size: usize },
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReceiptError::NoRules => {
                write!(f, "未提供任何验证规则：不允许出具通过收据")
            }
            ReceiptError::Collect(e) => write!(f, "差异采集失败：{e}"),
            ReceiptError::Verify(e) => write!(f, "验证规则错误：{e}"),
            ReceiptError::ReceiptTooLarge { size } => {
                write!(f, "收据 {size} 字节超过 64 KiB 上限，不截断、不发布")
            }
        }
    }
}

impl std::error::Error for ReceiptError {}

impl From<VerifyError> for ReceiptError {
    fn from(e: VerifyError) -> Self {
        ReceiptError::Verify(e)
    }
}

impl From<crate::diff::DiffError> for ReceiptError {
    fn from(e: crate::diff::DiffError) -> Self {
        ReceiptError::Collect(e)
    }
}

fn rule_status(outcome: &crate::verify::VerificationOutcome) -> (&'static str, Option<String>) {
    match outcome.status {
        crate::verify::VerificationStatus::Passed => ("pass", None),
        crate::verify::VerificationStatus::Failed => ("fail", Some("退出码非零".into())),
        crate::verify::VerificationStatus::Timeout => ("timeout", Some("超时".into())),
        crate::verify::VerificationStatus::ToolNotFound => {
            ("not_run", Some("工具未找到，规则未启动".into()))
        }
        crate::verify::VerificationStatus::NotRun => ("not_run", Some("规则未能启动".into())),
    }
}

/// 运行规则并生成测试收据。
///
/// 流程：验证前采集差异并与被测补丁核对（摘要不匹配 → failed 收据，
/// 原因"验证前差异与被测补丁不一致"）→ 逐规则执行 → 验证后再采集并
/// 比较（漂移 → failed 收据，原因"验证改动了工作副本"）。`diff_sha256`
/// 绑定验证时的实际差异。收据超过 64 KiB 时返回错误，不截断。
#[allow(clippy::too_many_arguments)]
pub fn run_and_build_receipt(
    config: &VerifyConfig,
    rule_ids: &[String],
    worktree: &Path,
    base_oid: &str,
    attempt: &str,
    tested: &CollectedDiff,
    scope: &WriteScope,
    started_at: &str,
) -> Result<TestReceipt, ReceiptError> {
    run_and_build_receipt_with(
        config, rule_ids, worktree, base_oid, attempt, tested, scope, started_at, run_rule,
    )
}

/// Delegate process supervision without changing the receipt rules and digest checks.
#[allow(clippy::too_many_arguments)]
pub fn run_and_build_receipt_with(
    config: &VerifyConfig,
    rule_ids: &[String],
    worktree: &Path,
    base_oid: &str,
    attempt: &str,
    tested: &CollectedDiff,
    scope: &WriteScope,
    started_at: &str,
    mut execute: impl FnMut(
        &VerifyConfig,
        &str,
        &Path,
    ) -> Result<crate::VerificationOutcome, VerifyError>,
) -> Result<TestReceipt, ReceiptError> {
    if rule_ids.is_empty() {
        return Err(ReceiptError::NoRules);
    }
    // 0. 第一条命令启动前：全量解析规则列表并核对工作目录——未知或
    //    未授权 ID 不允许任何规则启动。
    let mut resolved: Vec<&VerifyRule> = Vec::new();
    for rule_id in rule_ids {
        resolved.push(config.resolve(rule_id)?);
    }
    if !worktree.is_dir() {
        return Err(ReceiptError::Verify(VerifyError::WorkdirInvalid {
            path: worktree.to_owned(),
        }));
    }
    // 1. 验证前采集：被 Git 层拒绝的改动留下失败证据（不含文件内容）；
    //    实际差异与被测补丁不一致（摘要不匹配）→ failed 收据。两者都
    //    发生在任何规则启动之前，零调用。
    let pre = match collect_diff(worktree, base_oid, scope) {
        Ok(diff) => diff,
        Err(DiffError::Rejected(rejections)) => {
            return failed_receipt_without_rules(
                attempt,
                base_oid,
                "验证前差异被 Git 层拒绝（越权/链接/Gitlink/二进制/模式）",
                Some(*rejections),
                started_at,
            );
        }
        Err(other) => return Err(ReceiptError::Collect(other)),
    };
    let pre_sha = Some(sha256_hex(pre.patch.as_bytes()));
    let mut drift_reason: Option<String> = None;
    let mut any_failure = false;
    if pre.patch != tested.patch {
        // 摘要不匹配：任何规则都不得启动，直接出具 failed 收据（零调用）。
        let receipt = TestReceipt {
            attempt: attempt.to_owned(),
            base_oid: base_oid.to_owned(),
            diff_sha256: pre_sha,
            status: "failed",
            reason: Some("验证前差异与被测补丁不一致（摘要不匹配）".into()),
            rules: Vec::new(),
            post_rejections: None,
            json: String::new(),
        };
        let json = serialize_receipt(&receipt, None, None)?;
        return Ok(TestReceipt { json, ..receipt });
    }
    // 2. 逐规则执行（顺序与传入顺序一致）；工具缺失或启动失败转为
    //    not_run 条目并使收据 failed，不中断其余规则的记录。
    let mut rules: Vec<RuleReceiptEntry> = Vec::new();
    for rule_id in rule_ids {
        let outcome = match execute(config, rule_id, worktree) {
            Err(
                err @ (VerifyError::Stopped
                | VerifyError::ExitUnconfirmed(_)
                | VerifyError::ProcessFailed(_)),
            ) => return Err(err.into()),
            Ok(outcome) => outcome,
            Err(VerifyError::ToolNotFound { executable, .. }) => {
                any_failure = true;
                rules.push(RuleReceiptEntry {
                    rule_id: rule_id.clone(),
                    display_argv: vec![executable],
                    status: "not_run",
                    exit_code: None,
                    timed_out: false,
                    started_at: started_at.to_owned(),
                    elapsed_ms: 0,
                    output_excerpt: String::new(),
                    reason: Some("工具未找到，规则未启动".into()),
                });
                continue;
            }
            Err(err) => {
                any_failure = true;
                rules.push(RuleReceiptEntry {
                    rule_id: rule_id.clone(),
                    display_argv: Vec::new(),
                    status: "not_run",
                    exit_code: None,
                    timed_out: false,
                    started_at: started_at.to_owned(),
                    elapsed_ms: 0,
                    output_excerpt: String::new(),
                    reason: Some(format!("规则未能启动：{err}")),
                });
                continue;
            }
        };
        let (status, reason) = rule_status(&outcome);
        if status != "pass" {
            any_failure = true;
        }
        let mut display_argv = vec![outcome.executable.clone()];
        display_argv.extend(outcome.argv.iter().cloned());
        rules.push(RuleReceiptEntry {
            rule_id: outcome.rule_id,
            display_argv,
            status,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            started_at: started_at.to_owned(),
            elapsed_ms: outcome.elapsed_ms,
            output_excerpt: outcome.output_excerpt,
            reason,
        });
    }
    // 3. 验证后：差异漂移（源码被改写）→ failed；被 Git 层拒绝的改动
    //    留下不含文件内容的证据。
    let mut post_rejections: Option<crate::diff::Rejections> = None;
    let post = match collect_diff(worktree, base_oid, scope) {
        Ok(diff) => Some(diff),
        Err(DiffError::Rejected(rejections)) => {
            any_failure = true;
            drift_reason = Some("验证改动了工作副本且出现被拒绝的改动".into());
            post_rejections = Some(*rejections);
            None
        }
        Err(other) => return Err(ReceiptError::Collect(other)),
    };
    let post_sha = post
        .as_ref()
        .map(|diff| sha256_hex(diff.patch.as_bytes()))
        .unwrap_or_default();
    let mut source_modified = false;
    if let Some(diff) = &post {
        if diff.patch != pre.patch {
            source_modified = true;
            any_failure = true;
            drift_reason = Some("验证改动了工作副本（差异漂移）".into());
        }
    }
    // 4. 汇总：全部通过且无漂移才出具 pass 收据。
    let (status, reason) = if any_failure {
        (
            "failed",
            drift_reason
                .clone()
                .or_else(|| Some("存在失败或未通过的规则".into())),
        )
    } else {
        ("pass", None)
    };
    let receipt = TestReceipt {
        attempt: attempt.to_owned(),
        base_oid: base_oid.to_owned(),
        diff_sha256: pre_sha,
        status,
        reason: reason.clone(),
        rules,
        post_rejections,
        json: String::new(),
    };
    let json = serialize_receipt(&receipt, Some(&post_sha), Some(source_modified))?;
    if json.len() > MAX_RECEIPT_BYTES {
        return Err(ReceiptError::ReceiptTooLarge { size: json.len() });
    }
    Ok(TestReceipt { json, ..receipt })
}

/// 无规则执行的 failed 收据（预检阶段被拒绝；零调用，留证据不留内容）。
fn failed_receipt_without_rules(
    attempt: &str,
    base_oid: &str,
    reason: &str,
    rejections: Option<crate::diff::Rejections>,
    _started_at: &str,
) -> Result<TestReceipt, ReceiptError> {
    // 与主流程统一序列化：diff_sha256 / post_diff_sha256 为 null
    //（未采集，不填空补丁的哈希），保留安全拒绝证据（只含路径）。
    let receipt = TestReceipt {
        attempt: attempt.to_owned(),
        base_oid: base_oid.to_owned(),
        diff_sha256: None,
        status: "failed",
        reason: Some(reason.to_owned()),
        rules: Vec::new(),
        post_rejections: rejections,
        json: String::new(),
    };
    let json = serialize_receipt(&receipt, None, None)?;
    if json.len() > MAX_RECEIPT_BYTES {
        return Err(ReceiptError::ReceiptTooLarge { size: json.len() });
    }
    Ok(TestReceipt { json, ..receipt })
}

/// 字符串数组 → JSON 数组。
fn json_array(paths: &[String]) -> String {
    paths
        .iter()
        .map(|p| format!("\"{}\"", json_escape(p)))
        .collect::<Vec<_>>()
        .join(",")
}

/// 确定性 JSON 序列化（schema_version 1；只含受限字段，无原始
/// stdout/stderr 全文）。`post_sha` 与 `source_modified` 进入收据，
/// 供验证后差异漂移审计。
#[allow(clippy::needless_pass_by_value)]
fn serialize_receipt(
    receipt: &TestReceipt,
    post_sha: Option<&str>,
    source_modified: Option<bool>,
) -> Result<String, ReceiptError> {
    let opt_string = |value: Option<&str>| match value {
        Some(v) => format!("\"{}\"", json_escape(v)),
        None => "null".to_owned(),
    };
    let opt_bool = |value: &Option<bool>| match value {
        Some(v) => v.to_string(),
        None => "null".to_owned(),
    };
    let _ = (&opt_string, &opt_bool);
    let entries: Vec<String> = receipt
        .rules
        .iter()
        .map(|r| {
            let argv: Vec<String> = r
                .display_argv
                .iter()
                .map(|a| format!("\"{}\"", json_escape(a)))
                .collect();
            let reason = match &r.reason {
                Some(reason) => format!("\"{}\"", json_escape(reason)),
                None => "null".to_owned(),
            };
            let exit = match r.exit_code {
                Some(code) => code.to_string(),
                None => "null".to_owned(),
            };
            format!(
                "{{\"rule_id\":\"{}\",\"display_argv\":[{}],\"status\":\"{}\",\"exit_code\":{},\"timed_out\":{},\"started_at\":\"{}\",\"elapsed_ms\":{},\"output_excerpt\":\"{}\",\"reason\":{}}}",
                json_escape(&r.rule_id),
                argv.join(","),
                r.status,
                exit,
                r.timed_out,
                json_escape(&r.started_at),
                r.elapsed_ms,
                json_escape(&r.output_excerpt),
                reason
            )
        })
        .collect();
    let reason = match &receipt.reason {
        Some(reason) => format!("\"{}\"", json_escape(reason)),
        None => "null".to_owned(),
    };
    let post_rejections = match &receipt.post_rejections {
        Some(r) => format!(
            "{{\"unauthorized\":{},\"invalid_paths\":{},\"symlinks\":{},\"gitlinks\":{},\"binaries\":{},\"mode_changes\":{},\"unsupported_files\":{},\"oversized_files\":{},\"resource_limits\":{}}}",
            json_array(&r.unauthorized),
            json_array(&r.invalid_paths),
            json_array(&r.symlinks),
            json_array(&r.gitlinks),
            json_array(&r.binaries),
            json_array(&r.mode_changes),
            json_array(&r.unsupported_files),
            json_array(&r.oversized_files),
            json_array(&r.resource_limits)
        ),
        None => "null".to_owned(),
    };
    Ok(format!(
        "{{\"schema_version\":1,\"attempt\":{},\"base_oid\":{},\"diff_sha256\":{},\"post_diff_sha256\":{},\"source_modified\":{},\"status\":\"{}\",\"reason\":{},\"post_rejections\":{},\"rules\":[{}]}}",
        opt_string(Some(&receipt.attempt)),
        opt_string(Some(&receipt.base_oid)),
        opt_string(receipt.diff_sha256.as_deref()),
        post_sha
            .map(|sha| format!("\"{sha}\""))
            .unwrap_or_else(|| "null".to_owned()),
        opt_bool(&source_modified),
        receipt.status,
        reason,
        post_rejections,
        entries.join(",")
    ))
}
