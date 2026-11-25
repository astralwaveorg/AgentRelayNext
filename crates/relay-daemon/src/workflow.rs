//! 首版自动链路（15/16）：实现 → 本地验证 → 独立只读评审 → 可交付结果。
//!
//! 两个不同的本地适配器经与真实适配器相同的接口（probe | run）接入。
//! 评审只拿白名单 diff 与测试收据（15：不共用可写 worktree），结论按
//! ReviewReport v1 校验并绑定两个摘要；评审退回则自动返工一轮、再验证、
//! 再评审，到上限即停。轮次在单次 attempt 的 running 态内完成，03 的
//! 状态表不变；每轮记一条过程事件。
//!
//! 每轮在同一 attempt 内持久化独立 StageRun；制品绑定生成时的阶段和摘要。
//! 默认使用离线适配器。显式启用的 Claude CLI 夹具只允许访问本机合成端点。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::service::{
    classify_events, now_rfc3339, spawn_event_reader, AdapterProbeReport, AdapterRunEvents,
    M1Service, ReclaimOutcome, TEMPLATE_IMPLEMENTATION_ONLY, TEMPLATE_IMPLEMENT_REVIEW,
};
use relay_core::{Event, State};
use relay_git::{
    collect_diff, sha256_hex, CollectedDiff, TestReceipt, VerifyConfig, WorktreeReport, WriteScope,
};
use relay_store::{PlanRecord, TransitionInput};

/// 评审者的允许能力：只读（15：评审默认只提供 diff/收据，无写工具）。
const REVIEW_ALLOWED_CAPS: [&str; 1] = ["read_files"];
/// ReviewReport v1 上限（15：findings 最多 20 项，总体不超过 16 KiB）。
const MAX_FINDINGS: usize = 20;
const MAX_REPORT_BYTES: usize = 16 * 1024;

/// 目录中的一个适配器及其探测结果。
#[derive(Debug, Clone)]
pub struct AdapterCandidate {
    pub path: PathBuf,
    pub probe: AdapterProbeReport,
}

impl AdapterCandidate {
    /// 目录项文件名（预览与错误只显示名字，不回显任意路径）。
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// 预览用描述（16：说清各角色将用什么）。
    pub fn describe(&self) -> Value {
        json!({
            "adapter": self.name(),
            "identity": self.probe.binary,
            "model": self.probe.actual_model,
            "roles": self.probe.roles,
            "capabilities": self.probe.capabilities,
        })
    }
}

/// 选出的角色组合。
#[derive(Debug, Clone)]
pub struct SelectedPair {
    pub implementer: AdapterCandidate,
    /// None = implementation_only 模板（诊断/兼容，显式选择）。
    pub reviewer: Option<AdapterCandidate>,
    pub template: String,
}

impl SelectedPair {
    pub fn describe(&self) -> Value {
        json!({
            "template": self.template,
            "implement": self.implementer.describe(),
            "review": self.reviewer.as_ref().map(AdapterCandidate::describe),
            "reason": match &self.reviewer {
                Some(_) => "实现者可写、评审者只读，且工具与模型身份均不同，满足独立评审",
                None => "诊断模板：只有实现者，不构成独立评审，不用于自动验收",
            },
        })
    }
}

/// 选不出组合：每个排除项一个稳定原因（16）；探测退出未确认时带进程号。
#[derive(Debug, Clone)]
pub struct SelectionFailure {
    pub reasons: Vec<String>,
    pub unconfirmed_pgid: Option<i32>,
}

/// 列出目录中的适配器：常规可执行文件（拒绝符号链接与目录），按文件名
/// 排序——相同目录快照给出相同推荐（16）。
fn list_catalog(dir: &Path) -> Result<Vec<PathBuf>, String> {
    use std::os::unix::fs::PermissionsExt;
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_symlink() || !meta.is_file() {
            continue;
        }
        if meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

/// 独立性（15 默认严格评审策略）：可执行文件、工具标识、可核实的模型
/// 身份三者都不同。别名不算独立。
fn independence_problem(
    implementer: &AdapterCandidate,
    reviewer: &AdapterCandidate,
) -> Result<(), &'static str> {
    if implementer.path == reviewer.path {
        return Err("同一可执行文件");
    }
    if implementer.probe.binary == reviewer.probe.binary {
        return Err("同一工具");
    }
    if let (Some(left), Some(right)) = (
        &implementer.probe.actual_model,
        &reviewer.probe.actual_model,
    ) {
        if crate::model_catalog::normalize_model(left)
            == crate::model_catalog::normalize_model(right)
        {
            return Err("同一模型，别名不算独立");
        }
    }
    if implementer.probe.actual_model == reviewer.probe.actual_model {
        return Err("同一模型，别名不算独立");
    }
    Ok(())
}

/// 评审结论（经校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Pass,
    ChangesRequested,
}

/// 评审报告校验结果。
#[derive(Debug, Clone)]
pub struct ReviewEval {
    pub verdict: ReviewVerdict,
    pub findings: Vec<Value>,
    /// 报告给出 pass 但附有阻断 finding：按退回处理，不报成功。
    pub pass_with_blocking: bool,
}

/// 按 ReviewReport v1 校验评审报告（15）：结构、规模、摘要绑定、结论
/// 与 findings 的一致性。只有对应当前候选、结构合法的报告才被采纳；
/// 评审回执不能为新补丁复用。
pub fn evaluate_review_report(
    report: &Value,
    diff_sha: &str,
    receipt_sha: &str,
) -> Result<ReviewEval, String> {
    let Some(report) = report.as_object() else {
        return Err("评审报告不是 JSON 对象".to_owned());
    };
    if report.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("评审报告 schema_version 不是 1".to_owned());
    }
    let size = serde_json::to_string(report)
        .map(|text| text.len())
        .unwrap_or(usize::MAX);
    if size > MAX_REPORT_BYTES {
        return Err(format!("评审报告 {size} 字节超过 {MAX_REPORT_BYTES} 上限"));
    }
    let field = |key: &str| -> Result<String, String> {
        report
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("评审报告缺少字符串字段 {key}"))
    };
    let candidate = field("candidate_diff_sha256")?;
    if candidate != diff_sha {
        return Err("评审报告绑定的补丁摘要与当前候选不符（评审回执不能为新补丁复用）".to_owned());
    }
    let tested = field("test_receipt_sha256")?;
    if tested != receipt_sha {
        return Err("评审报告绑定的测试收据摘要与当前候选不符".to_owned());
    }
    let verdict = field("verdict")?;
    let findings = match report.get("findings") {
        Some(Value::Array(items)) => items.clone(),
        _ => return Err("评审报告的 findings 必须是数组".to_owned()),
    };
    if findings.len() > MAX_FINDINGS {
        return Err(format!(
            "评审报告 findings {} 项超过 {MAX_FINDINGS} 项上限",
            findings.len()
        ));
    }
    let mut blocking = 0usize;
    for (index, finding) in findings.iter().enumerate() {
        let Some(finding) = finding.as_object() else {
            return Err(format!("findings[{index}] 不是对象"));
        };
        let severity = finding
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(severity, "blocking" | "major" | "minor" | "info") {
            return Err(format!(
                "findings[{index}] 的 severity 不合法：{severity:?}"
            ));
        }
        if severity == "blocking" {
            blocking += 1;
        }
        for key in ["evidence", "suggestion"] {
            if finding.get(key).and_then(Value::as_str).is_none() {
                return Err(format!("findings[{index}] 缺少字符串字段 {key}"));
            }
        }
        for key in ["path", "location"] {
            if let Some(value) = finding.get(key) {
                if !value.is_string() && !value.is_null() {
                    return Err(format!("findings[{index}] 的 {key} 须为字符串或 null"));
                }
            }
        }
    }
    match verdict.as_str() {
        "pass" => {
            if blocking > 0 {
                return Ok(ReviewEval {
                    verdict: ReviewVerdict::ChangesRequested,
                    findings,
                    pass_with_blocking: true,
                });
            }
            Ok(ReviewEval {
                verdict: ReviewVerdict::Pass,
                findings,
                pass_with_blocking: false,
            })
        }
        "changes_requested" => {
            if findings.is_empty() {
                return Err("changes_requested 必须至少有一项可执行 finding，报告无效".to_owned());
            }
            Ok(ReviewEval {
                verdict: ReviewVerdict::ChangesRequested,
                findings,
                pass_with_blocking: false,
            })
        }
        "inconclusive" => Err("评审结论不确定（inconclusive），需人工处理".to_owned()),
        other => Err(format!("评审结论不合法：{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    fn report(findings: Value) -> Value {
        json!({
            "schema_version": 1,
            "candidate_diff_sha256": SHA,
            "test_receipt_sha256": SHA,
            "verdict": "changes_requested",
            "findings": findings,
        })
    }

    fn finding(severity: &str) -> Value {
        json!({"severity": severity, "path": "src/count.rs", "location": null,
               "evidence": "e", "suggestion": "s"})
    }

    /// 结构规则：finding 必须是带合法 severity 与证据/建议的对象；
    /// path/location 可空但须为字符串；上限 20 项。
    #[test]
    fn structural_rules_are_enforced() {
        assert!(evaluate_review_report(&report(json!([finding("major")])), SHA, SHA).is_ok());
        let not_object =
            evaluate_review_report(&report(json!(["just text"])), SHA, SHA).unwrap_err();
        assert!(not_object.contains("不是对象"), "{not_object}");
        let bad_severity =
            evaluate_review_report(&report(json!([finding("critical")])), SHA, SHA).unwrap_err();
        assert!(bad_severity.contains("severity"), "{bad_severity}");
        let mut missing_evidence = finding("major");
        missing_evidence.as_object_mut().unwrap().remove("evidence");
        let err = evaluate_review_report(&report(json!([missing_evidence])), SHA, SHA).unwrap_err();
        assert!(err.contains("evidence"), "{err}");
        let mut bad_path = finding("major");
        bad_path["path"] = json!(42);
        let err = evaluate_review_report(&report(json!([bad_path])), SHA, SHA).unwrap_err();
        assert!(err.contains("path"), "{err}");
        let many: Vec<Value> = (0..21).map(|_| finding("minor")).collect();
        let err = evaluate_review_report(&report(json!(many)), SHA, SHA).unwrap_err();
        assert!(err.contains("上限"), "{err}");
        // findings 不是数组 → 无效。
        let mut not_array = report(json!([]));
        not_array["findings"] = json!("none");
        assert!(evaluate_review_report(&not_array, SHA, SHA).is_err());
    }

    /// 独立性：可执行文件、工具标识、模型身份任一相同即不独立。
    #[test]
    fn independence_requires_tool_and_model_to_differ() {
        let candidate = |path: &str, binary: &str, model: &str| AdapterCandidate {
            path: PathBuf::from(path),
            probe: AdapterProbeReport {
                binary: binary.to_owned(),
                actual_model: Some(model.to_owned()),
                roles: vec!["x".to_owned()],
                capabilities: vec![],
                version: String::new(),
            },
        };
        assert_eq!(
            independence_problem(
                &candidate("a", "tool-a", "m1"),
                &candidate("b", "tool-b", "m2")
            ),
            Ok(())
        );
        assert_eq!(
            independence_problem(
                &candidate("a", "tool-a", "deepseek-v4.1-flash"),
                &candidate("b", "tool-b", "opencode-go/deepseek-v4.1-flash")
            )
            .unwrap_err(),
            "同一模型，别名不算独立"
        );
        assert_eq!(
            independence_problem(
                &candidate("a", "tool-a", "m1"),
                &candidate("b", "tool-b", "m1")
            )
            .unwrap_err(),
            "同一模型，别名不算独立"
        );
        assert_eq!(
            independence_problem(&candidate("a", "tool", "m1"), &candidate("b", "tool", "m2"))
                .unwrap_err(),
            "同一工具"
        );
    }
}

/// 一次受派发门约束的子进程运行结果。
pub(crate) struct TrackedRun {
    pub exit_ok: bool,
    pub events: AdapterRunEvents,
}

/// 受控子进程运行失败。`Unconfirmed` 表示退出不明：进程号留在待确认
/// 标记上，gate 保持 pending；其余变体表示进程已确认退出（或从未启动）。
#[derive(Debug)]
pub(crate) enum TrackedFailure {
    /// 子进程未能启动（从未启动）。
    SpawnFailed(String),
    /// 退出不明：进程组仍可能存活。
    Unconfirmed(String),
    /// 等待上限已到，已回收并确认退出。
    TimedOut,
    /// 任务不再处于 running（取消/中断）：已回收并确认退出，后继停止。
    Cancelled,
    /// daemon 停止：已回收并确认退出，不做状态推进。
    Stopped,
    /// 协议不合规（进程已确认退出）。
    Protocol(String),
}

/// 一轮自动链路的产出：最终通过评审的候选、收据与 handoff 摘要。
pub(crate) struct WorkflowOutput {
    pub collected: CollectedDiff,
    pub receipt: TestReceipt,
    pub started_at: String,
    pub summary: Value,
}

impl M1Service {
    /// 按角色能力从目录选出组合（16）。先过滤（角色、能力、模型身份），
    /// 再枚举完整的实现者 × 评审者配对（不逐列贪心：一个合格的实现位
    /// 不能挤掉唯一合格的评审位）。`gate` 给出 attempt 时（派发路径）
    /// 探测受派发门约束：任一探测退出未确认即停止后续探测并返回其进程号。
    pub(crate) fn select_pair(
        &self,
        dir: &Path,
        gate: Option<(&str, u32)>,
    ) -> Result<SelectedPair, SelectionFailure> {
        let fail = |reasons: Vec<String>, unconfirmed_pgid: Option<i32>| {
            Err(SelectionFailure {
                reasons,
                unconfirmed_pgid,
            })
        };
        let entries = match list_catalog(dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("relay daemon: 无法读取适配器目录 {}：{e}", dir.display());
                return fail(vec!["无法读取适配器目录".to_owned()], None);
            }
        };
        if entries.is_empty() {
            return fail(vec!["适配器目录中没有可执行条目".to_owned()], None);
        }
        let name_of = |path: &Path| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        let mut candidates: Vec<AdapterCandidate> = Vec::new();
        let mut probe_failures: Vec<String> = Vec::new();
        for path in entries {
            match self.adapter_probe(&path, gate) {
                Ok(probe) => candidates.push(AdapterCandidate { path, probe }),
                Err(e) => {
                    if e.group_unconfirmed {
                        eprintln!(
                            "relay daemon: 目录项 {} 的探测进程退出未确认（pgid {:?}）",
                            name_of(&path),
                            e.pgid
                        );
                        return fail(
                            vec![format!(
                                "{}：探测进程退出未确认，需人工核对",
                                name_of(&path)
                            )],
                            e.pgid,
                        );
                    }
                    probe_failures.push(format!("{}：探测失败（{}）", name_of(&path), e.message));
                }
            }
        }
        // 过滤：阶段必需能力与可核实的模型身份（厂商自报只供候选探索）。
        let allowed_write = self.config.allowed_capabilities.clone();
        let mut implementers: Vec<AdapterCandidate> = Vec::new();
        let mut reviewers: Vec<AdapterCandidate> = Vec::new();
        let mut implementer_reasons = probe_failures.clone();
        let mut reviewer_reasons = probe_failures;
        for candidate in &candidates {
            let name = candidate.name();
            let roles = if candidate.probe.roles.is_empty() {
                "未声明".to_owned()
            } else {
                candidate.probe.roles.join(", ")
            };
            if !candidate.probe.roles.iter().any(|r| r == "implement") {
                implementer_reasons.push(format!("{name}：无实现角色（角色：{roles}）"));
            } else {
                let unauthorized: Vec<String> = candidate
                    .probe
                    .capabilities
                    .iter()
                    .filter(|cap| !allowed_write.contains(cap))
                    .cloned()
                    .collect();
                if !unauthorized.is_empty() {
                    implementer_reasons.push(format!(
                        "{name}：实现能力未授权（{}）",
                        unauthorized.join(", ")
                    ));
                } else if candidate.probe.actual_model.is_none() {
                    implementer_reasons.push(format!("{name}：模型身份未核实，不能作为实现者"));
                } else if let Some(expected) = &self.config.expected_model {
                    if candidate.probe.actual_model.as_deref() != Some(expected.as_str()) {
                        implementer_reasons
                            .push(format!("{name}：模型身份与固定配置不符（固定 {expected}）"));
                    } else {
                        implementers.push(candidate.clone());
                    }
                } else {
                    implementers.push(candidate.clone());
                }
            }
            if !candidate.probe.roles.iter().any(|r| r == "review") {
                reviewer_reasons.push(format!("{name}：无评审角色（角色：{roles}）"));
            } else {
                let writable: Vec<String> = candidate
                    .probe
                    .capabilities
                    .iter()
                    .filter(|cap| !REVIEW_ALLOWED_CAPS.contains(&cap.as_str()))
                    .cloned()
                    .collect();
                if !writable.is_empty() {
                    reviewer_reasons.push(format!(
                        "{name}：评审必须只读，声明了 {} 能力",
                        writable.join(", ")
                    ));
                } else if candidate.probe.actual_model.is_none() {
                    reviewer_reasons.push(format!("{name}：模型身份未核实，不能作为独立评审"));
                } else {
                    reviewers.push(candidate.clone());
                }
            }
        }
        if self.config.workflow_template == TEMPLATE_IMPLEMENTATION_ONLY {
            return match implementers.first() {
                Some(implementer) => Ok(SelectedPair {
                    implementer: implementer.clone(),
                    reviewer: None,
                    template: TEMPLATE_IMPLEMENTATION_ONLY.to_owned(),
                }),
                None => fail(implementer_reasons, None),
            };
        }
        if let Some(fixture) = &self.config.claude_review_fixture {
            // 显式选择夹具时不回退到目录里的替身评审者。
            reviewers = vec![AdapterCandidate {
                path: fixture.executable.clone(),
                probe: fixture.registration("review"),
            }];
        }
        if let Some(fixture) = &self.config.opencode_review_fixture {
            reviewers = vec![AdapterCandidate {
                path: fixture.executable.clone(),
                probe: fixture.registration(false),
            }];
        }
        let managed_implementer = self
            .config
            .claude_implement_fixture
            .as_ref()
            .map(|fixture| {
                (
                    fixture.executable.clone(),
                    fixture.registration("implement"),
                )
            })
            .or_else(|| {
                self.config
                    .opencode_implement_fixture
                    .as_ref()
                    .map(|fixture| (fixture.executable.clone(), fixture.registration(true)))
            });
        if let Some((path, registration)) = managed_implementer {
            if registration
                .capabilities
                .iter()
                .any(|cap| !allowed_write.contains(cap))
                || self
                    .config
                    .expected_model
                    .as_ref()
                    .is_some_and(|expected| registration.actual_model.as_ref() != Some(expected))
            {
                return fail(
                    vec!["受管实现缺少授权能力或固定模型不匹配".to_owned()],
                    None,
                );
            }
            implementers = vec![AdapterCandidate {
                path,
                probe: registration,
            }];
        }
        // 联合检查：枚举完整组合，取目录顺序里第一个满足独立性的配对。
        let mut pair_reasons = Vec::new();
        for implementer in &implementers {
            for reviewer in &reviewers {
                match independence_problem(implementer, reviewer) {
                    Ok(()) => {
                        return Ok(SelectedPair {
                            implementer: implementer.clone(),
                            reviewer: Some(reviewer.clone()),
                            template: TEMPLATE_IMPLEMENT_REVIEW.to_owned(),
                        })
                    }
                    Err(why) => pair_reasons.push(format!(
                        "{} + {}：不独立（{why}）",
                        implementer.name(),
                        reviewer.name()
                    )),
                }
            }
        }
        let mut reasons = Vec::new();
        reasons.extend(implementer_reasons);
        reasons.extend(reviewer_reasons);
        reasons.extend(pair_reasons);
        reasons.push("没有同时满足能力、只读与独立性要求的实现者 + 评审者组合".to_owned());
        fail(reasons, None)
    }

    /// 运行一个受派发门约束的子进程（评审者、返工实现者）：进程组记到
    /// 待确认标记上；有界等待、读取与回收；组长退出后确认整组退出才
    /// 清除进程号。取消/停止/任务不再 running 时回收并结束。
    fn run_tracked(
        &self,
        exe: &Path,
        arg: &Path,
        envs: &BTreeMap<String, String>,
        task_id: &str,
        attempt_no: u32,
        stop: &AtomicBool,
    ) -> Result<TrackedRun, TrackedFailure> {
        let mut command = Command::new(exe);
        command.arg("run").arg(arg).envs(envs);
        self.run_tracked_command(command, false, task_id, attempt_no, stop)
    }

    fn run_tracked_command(
        &self,
        mut command: Command,
        claude: bool,
        task_id: &str,
        attempt_no: u32,
        stop: &AtomicBool,
    ) -> Result<TrackedRun, TrackedFailure> {
        use std::os::unix::process::CommandExt;
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| TrackedFailure::SpawnFailed(format!("无法启动受管子进程：{e}")))?;
        let pgid = child.id() as i32;
        let now = || now_rfc3339();
        // 归属证据：登记失败即失去可核实的进程号身份——保守回收。
        if let Err(e) = self.store.set_gate_pgid(task_id, attempt_no, pgid, &now()) {
            let confirmed = self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
            self.reap_child(&mut child);
            return Err(if confirmed {
                TrackedFailure::Protocol(format!("子进程组登记失败：{e}"))
            } else {
                TrackedFailure::Unconfirmed(format!("子进程组登记失败且退出未确认：{e}"))
            });
        }
        let events: Arc<Mutex<AdapterRunEvents>> =
            Arc::new(Mutex::new(AdapterRunEvents::default()));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let read_budget = Duration::from_millis(self.config.adapter_read_timeout_ms);
        let stdout = child.stdout.take().expect("tracked stdout piped");
        let reader = if claude {
            crate::claude_review::spawn_reader(stdout, Arc::clone(&events), done_tx, read_budget)
        } else {
            spawn_event_reader(
                stdout,
                Arc::clone(&events),
                done_tx,
                self.config.adapter_max_frame_bytes,
                read_budget,
            )
        };
        let deadline = Instant::now() + Duration::from_millis(self.config.worker_timeout_ms);
        // 回收并结算：确认整组退出才清除标记上的进程号。
        let reclaim_settled = |child: &mut std::process::Child| -> bool {
            let confirmed = self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
            self.reap_child(child);
            if confirmed {
                let _ = self.store.set_gate_pgid(task_id, attempt_no, 0, &now());
            }
            confirmed
        };
        let mut exit_ok = false;
        let mut running = true;
        while running {
            if stop.load(Ordering::SeqCst) {
                let confirmed = reclaim_settled(&mut child);
                if !confirmed {
                    return Err(TrackedFailure::Unconfirmed(
                        "daemon 停止回收未确认退出".into(),
                    ));
                }
                // 停机：受控进程均已确认退出，本 attempt 不再启动子进程。
                let _ = self.store.release_dispatch_gate(
                    task_id,
                    attempt_no,
                    "停机回收已确认退出",
                    &now(),
                );
                return Err(TrackedFailure::Stopped);
            }
            if let Ok(Some(t)) = self.store.get_task(task_id) {
                if t.state != State::Running {
                    // 取消（或已被取消/中断）：后继活动立即停止。
                    let confirmed = reclaim_settled(&mut child);
                    if !confirmed {
                        return Err(TrackedFailure::Unconfirmed(
                            "取消回收未确认退出，保持阻断".into(),
                        ));
                    }
                    self.confirm_workers_stopped(task_id, attempt_no);
                    let _ = self.store.release_dispatch_gate(
                        task_id,
                        attempt_no,
                        "取消回收已确认退出",
                        &now(),
                    );
                    return Err(TrackedFailure::Cancelled);
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_ok = status.success();
                    running = false;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        if !reclaim_settled(&mut child) {
                            return Err(TrackedFailure::Unconfirmed(
                                "等待上限已到，整组退出未确认".into(),
                            ));
                        }
                        return Err(TrackedFailure::TimedOut);
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(e) => {
                    let confirmed = reclaim_settled(&mut child);
                    return Err(if confirmed {
                        TrackedFailure::Protocol(format!("等待子进程失败：{e}"))
                    } else {
                        TrackedFailure::Unconfirmed("等待子进程失败且退出未确认".into())
                    });
                }
            }
        }
        // 组长退出 ≠ 整组退出：确认后清除进程号。
        if !reclaim_settled(&mut child) {
            return Err(TrackedFailure::Unconfirmed(
                "组长退出但整组退出未确认（成员残留）".into(),
            ));
        }
        // 有界等待事件读完（读者线程自有上限，join 不会无期限阻塞）。
        if done_rx.recv_timeout(read_budget).is_err() {
            events.lock().expect("adapter events 锁中毒").read_timeouts += 1;
        }
        let _ = reader.join();
        let events = events.lock().expect("adapter events 锁中毒").clone();
        Ok(TrackedRun { exit_ok, events })
    }

    /// 自动链路失败收口（受控子进程均已确认退出）：解封 gate、任务
    /// failed、记过程事件。返回 None 供调用方结束 attempt。
    pub(crate) fn workflow_fail(
        &self,
        task_id: &str,
        attempt_no: u32,
        revision: u64,
        reason: &str,
    ) -> Option<WorkflowOutput> {
        let _ = self.store.release_dispatch_gate(
            task_id,
            attempt_no,
            "自动链路失败：受控子进程均已确认退出",
            &now_rfc3339(),
        );
        self.fail_worker(task_id, attempt_no, revision, reason);
        self.confirm_workers_stopped(task_id, attempt_no);
        let _ = self.store.append_event(
            task_id,
            attempt_no,
            "workflow_failed",
            Some(reason),
            &now_rfc3339(),
        );
        None
    }

    /// 退出不明收口：gate 保持 pending（进程号已留存），任务 interrupted，
    /// 重试阻断，直到确认退出。
    fn workflow_interrupt(
        &self,
        task_id: &str,
        attempt_no: u32,
        revision: u64,
        reason: &str,
    ) -> Option<WorkflowOutput> {
        let _ = self.store.set_gate_reason(
            task_id,
            attempt_no,
            &format!("自动链路子进程退出未确认：{reason}"),
            &now_rfc3339(),
        );
        let _ = self.store.transition(TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision: revision,
            event: Event::MarkInterrupted,
            now: &now_rfc3339(),
        });
        let _ = self.store.append_event(
            task_id,
            attempt_no,
            "workflow_interrupted",
            Some(reason),
            &now_rfc3339(),
        );
        None
    }

    /// 把受控子进程失败映射为收口动作。
    fn handle_tracked_failure(
        &self,
        task_id: &str,
        attempt_no: u32,
        revision: u64,
        failure: TrackedFailure,
        what: &str,
    ) -> Option<WorkflowOutput> {
        match failure {
            TrackedFailure::Unconfirmed(detail) => {
                self.workflow_interrupt(task_id, attempt_no, revision, &format!("{what}：{detail}"))
            }
            // 取消/停止/中断：状态与 gate 已在运行循环内处理。
            TrackedFailure::Cancelled | TrackedFailure::Stopped => None,
            TrackedFailure::SpawnFailed(detail) | TrackedFailure::Protocol(detail) => {
                self.workflow_fail(task_id, attempt_no, revision, &format!("{what}：{detail}"))
            }
            TrackedFailure::TimedOut => self.workflow_fail(
                task_id,
                attempt_no,
                revision,
                &format!("{what}：超过等待上限"),
            ),
        }
    }

    /// 任务仍在 running 才允许启动后继子进程（15：取消压住后继派发）。
    /// 不在 running 时解封 gate（受控进程均已确认退出）并返回 false。
    pub(crate) fn ensure_still_running(&self, task_id: &str, attempt_no: u32) -> bool {
        match self.store.get_task(task_id) {
            Ok(Some(t)) if t.state == State::Running && t.active_attempt == attempt_no => true,
            Ok(Some(t)) if t.active_attempt == attempt_no && t.state != State::Interrupted => {
                self.confirm_workers_stopped(task_id, attempt_no);
                let _ = self.store.release_dispatch_gate(
                    task_id,
                    attempt_no,
                    "任务已离开 running：受控子进程均已确认退出",
                    &now_rfc3339(),
                );
                false
            }
            _ => false,
        }
    }

    /// 自动链路轮次循环（15）：每轮 = 本地验证 → 独立只读评审；评审退回
    /// 且未到返工上限时，返工是新一轮实现/验证/评审。验证失败不送评审。
    /// 评审输入只有 diff 与收据；评审通过后再次采集差异，候选与被审
    /// 候选不一致（评审写入或候选被改）不得放行。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_review_workflow(
        &self,
        task_id: &str,
        attempt_no: u32,
        attempt_id: &str,
        worktree: &WorktreeReport,
        plan: &PlanRecord,
        scope: &WriteScope,
        verify_config: &VerifyConfig,
        pair: &SelectedPair,
        stop: &AtomicBool,
    ) -> Option<WorkflowOutput> {
        let reviewer = pair
            .reviewer
            .as_ref()
            .expect("run_review_workflow 需要评审者");
        let revision = self
            .store
            .get_task(task_id)
            .ok()
            .flatten()
            .map(|t| t.revision)?;
        if !self.ensure_still_running(task_id, attempt_no) {
            return None;
        }
        use crate::verify_process::VerifyProcess;
        use relay_git::{receipt::run_and_build_receipt_with, verify::run_rule_supervised};
        use relay_git::{ReceiptError, VerifyError};
        use relay_store::{StageFinish, StageKey, StageKind};
        let supervisor = VerifyProcess {
            service: self,
            task_id,
            attempt_no,
            stop,
        };
        let key = |round, kind| StageKey {
            task_id,
            attempt_no,
            round,
            kind,
        };
        let fail = |reason: String| self.workflow_fail(task_id, attempt_no, revision, &reason);
        let max_rounds = 1 + self.config.max_rework.min(10);
        let review_root = self.config.managed_root.join("review").join(attempt_id);
        let note = |kind: &str, reason: String| {
            let _ =
                self.store
                    .append_event(task_id, attempt_no, kind, Some(&reason), &now_rfc3339());
        };
        let mut round = 1;
        loop {
            if stop.load(Ordering::SeqCst) || !self.ensure_still_running(task_id, attempt_no) {
                return None;
            }
            let verify = key(round, StageKind::Verify);
            if let Err(e) = self
                .store
                .begin_stage(verify, revision, None, &now_rfc3339())
            {
                return fail(format!("第 {round} 轮验证没有阶段启动许可：{e}"));
            }
            // ---- 本地验证（16：直接执行批准的本地验证命令，不调用模型）----
            note("verify_round", format!("round={round}"));
            let collected = match collect_diff(&worktree.path, &plan.base_oid, scope) {
                Ok(c) => c,
                Err(e) => {
                    return self.workflow_fail(
                        task_id,
                        attempt_no,
                        revision,
                        &format!("第 {round} 轮差异采集失败：{e}"),
                    )
                }
            };
            let started_at = now_rfc3339();
            let receipt = match run_and_build_receipt_with(
                verify_config,
                &plan.rule_ids,
                &worktree.path,
                &plan.base_oid,
                attempt_id,
                &collected,
                scope,
                &started_at,
                |config, rule, worktree| run_rule_supervised(config, rule, worktree, &supervisor),
            ) {
                Ok(r) => r,
                Err(ReceiptError::Verify(VerifyError::Stopped)) => {
                    supervisor.release_after_stop();
                    return None;
                }
                Err(ReceiptError::Verify(VerifyError::ExitUnconfirmed(reason))) => {
                    supervisor.keep_gate_interrupted(&reason);
                    return None;
                }
                Err(e) => {
                    return self.workflow_fail(
                        task_id,
                        attempt_no,
                        revision,
                        &format!("第 {round} 轮收据生成失败：{e}"),
                    )
                }
            };
            if receipt.status != "pass" {
                note(
                    "verify_failed",
                    format!("round={round};status={}", receipt.status),
                );
                return self.workflow_fail(
                    task_id,
                    attempt_no,
                    revision,
                    &format!("第 {round} 轮本地验证未通过，不送评审"),
                );
            }
            let diff_sha = sha256_hex(collected.patch.as_bytes());
            let receipt_sha = sha256_hex(receipt.json.as_bytes());
            if let Err(e) = self.store.finish_stage(
                verify,
                revision,
                StageFinish::Verified {
                    diff: &diff_sha,
                    receipt: &receipt_sha,
                },
                &now_rfc3339(),
            ) {
                return fail(format!("第 {round} 轮验证阶段无法提交：{e}"));
            }
            // ---- 评审输入：仅白名单 diff 与测试收据，不提供 worktree ----
            let review_dir = review_root.join(format!("round-{round}"));
            let write_input = std::fs::create_dir_all(&review_dir).and_then(|_| {
                std::fs::write(review_dir.join("diff.patch"), &collected.patch)?;
                std::fs::write(review_dir.join("test_receipt.json"), &receipt.json)
            });
            if let Err(e) = write_input {
                return self.workflow_fail(
                    task_id,
                    attempt_no,
                    revision,
                    &format!("第 {round} 轮评审输入写入失败：{e}"),
                );
            }
            if stop.load(Ordering::SeqCst) || !self.ensure_still_running(task_id, attempt_no) {
                return None;
            }
            let review = key(round, StageKind::Review);
            if let Err(e) = self.store.begin_stage(
                review,
                revision,
                Some((&diff_sha, &receipt_sha)),
                &now_rfc3339(),
            ) {
                return fail(format!("第 {round} 轮评审没有阶段启动许可：{e}"));
            }
            // ---- 独立只读评审 ----
            note(
                "review_round",
                format!("round={round};reviewer={}", reviewer.name()),
            );
            let result = if self.config.claude_review_fixture.is_some()
                || self.config.opencode_review_fixture.is_some()
                || self.config.live_routes.is_some()
            {
                let payload = match crate::route_payload::review_candidate(
                    &plan.goal,
                    &collected.patch,
                    &receipt.json,
                ) {
                    Ok(input) => input,
                    Err(code) => return self.workflow_fail(task_id, attempt_no, revision, code),
                };
                let command = if let Some(routes) = &self.config.live_routes {
                    routes.command(
                        crate::live_routes::Stage::Review,
                        &self.config.repo_root,
                        &self.config.managed_root,
                        &payload,
                    )
                } else if let Some(fixture) = &self.config.claude_review_fixture {
                    fixture.review_command(
                        &review_dir,
                        &self.config.repo_root,
                        &self.config.managed_root,
                        &payload,
                    )
                } else {
                    self.config
                        .opencode_review_fixture
                        .as_ref()
                        .expect("OpenCode fixture")
                        .command_with_payload(
                            &review_dir,
                            &self.config.repo_root,
                            &self.config.managed_root,
                            &payload,
                            false,
                        )
                };
                match command {
                    Ok(command) => self.run_tracked_command(
                        command,
                        self.config.claude_review_fixture.is_some(),
                        task_id,
                        attempt_no,
                        stop,
                    ),
                    Err(error) => Err(TrackedFailure::SpawnFailed(error.to_string())),
                }
            } else {
                self.run_tracked(
                    &reviewer.path,
                    &review_dir,
                    &self.config.review_env,
                    task_id,
                    attempt_no,
                    stop,
                )
            };
            let run = match result {
                Ok(run) => run,
                Err(failure) => {
                    return self.handle_tracked_failure(
                        task_id,
                        attempt_no,
                        revision,
                        failure,
                        "评审子进程",
                    )
                }
            };
            if let Err(e) = classify_events(&run.events, run.exit_ok) {
                self.fail_adapter(task_id, attempt_no, revision, &format!("评审：{e}"));
                return None;
            }
            if run.events.review_verdict_count != 1 {
                return self.workflow_fail(
                    task_id,
                    attempt_no,
                    revision,
                    &format!(
                        "评审结论帧必须恰好一个（收到 {} 个）",
                        run.events.review_verdict_count
                    ),
                );
            }
            if let Some(routes) = self.config.live_routes.as_ref() {
                if let Err(code) = crate::live_routes::record_usage(
                    &self.store,
                    task_id,
                    attempt_no,
                    crate::live_routes::Stage::Review,
                    run.events.provider_evidence.as_ref(),
                    Some(routes.review_model()),
                    routes.review_fallback(),
                ) {
                    self.fail_adapter(task_id, attempt_no, revision, &format!("评审：{code}"));
                    return None;
                }
            }
            let report = run
                .events
                .review_verdict
                .clone()
                .expect("review_verdict_count == 1");
            let eval = match evaluate_review_report(&report, &diff_sha, &receipt_sha) {
                Ok(eval) => eval,
                Err(e) => {
                    note("review_invalid", format!("round={round};{e}"));
                    return self.workflow_fail(
                        task_id,
                        attempt_no,
                        revision,
                        &format!("第 {round} 轮评审报告无效：{e}"),
                    );
                }
            };
            // 通过和退回都必须对应评审前的候选，避免把评审写入混进返工。
            let unchanged = match collect_diff(&worktree.path, &plan.base_oid, scope) {
                Ok(after) => after.patch == collected.patch,
                Err(_) => false,
            };
            if !unchanged {
                note(
                    "review_candidate_changed",
                    format!("round={round};候选在评审后发生变化"),
                );
                return self.workflow_fail(
                    task_id,
                    attempt_no,
                    revision,
                    "评审后候选发生变化（评审写入或候选被改），重审前不得放行",
                );
            }
            // 返工只能使用已经成功写入的评审报告。
            let saved = serde_json::to_vec_pretty(&report)
                .map_err(std::io::Error::other)
                .and_then(|bytes| std::fs::write(review_dir.join("review.json"), bytes));
            if let Err(e) = saved {
                return self.workflow_fail(
                    task_id,
                    attempt_no,
                    revision,
                    &format!("第 {round} 轮评审报告写入失败：{e}"),
                );
            }
            if let Err(e) = self.store.finish_stage(
                review,
                revision,
                StageFinish::Reviewed {
                    needs_changes: matches!(eval.verdict, ReviewVerdict::ChangesRequested),
                },
                &now_rfc3339(),
            ) {
                return fail(format!("第 {round} 轮评审阶段无法提交：{e}"));
            }
            let report_for_rework = json!({
                "candidate_diff_sha256":diff_sha, "test_receipt_sha256":receipt_sha,
                "findings":eval.findings,
            });
            match eval.verdict {
                ReviewVerdict::Pass => {
                    note("review_passed", format!("round={round}"));
                    let findings: Vec<Value> = eval
                        .findings
                        .iter()
                        .map(|f| {
                            json!({
                                "severity": f["severity"],
                                "path": f["path"],
                                "evidence": f["evidence"].as_str().map(|e| e.chars().take(200).collect::<String>()),
                            })
                        })
                        .collect();
                    let live = self.config.live_routes.is_some();
                    let limitations = if live {
                        vec![
                            "真实路线：实现与评审的实际模型由供应商响应核对；MiniMax 扣费归属仍为 unverified",
                            "评审输入只含目标、补丁、验证状态和摘要；不发送完整工作区或终端输出",
                            "阶段启动与结论独立持久化；重启后不自动重放中断阶段",
                            "评审只读由输入隔离与候选比对保证，不依赖文件系统权限",
                        ]
                    } else {
                        vec![
                            "离线适配器：不证明真实路线的权限、模型身份或套餐资格",
                            "阶段启动与结论独立持久化；重启后不自动重放中断阶段",
                            "评审只读由输入隔离与候选比对保证，不依赖文件系统权限",
                        ]
                    };
                    let summary = json!({
                        "template": pair.template,
                        "route": if live { "live" } else { "offline" },
                        "rounds": round,
                        "reworks": round - 1,
                        "max_rework": self.config.max_rework,
                        "routing": {
                            "implement": pair.implementer.describe(),
                            "review": reviewer.describe(),
                        },
                        "review": {
                            "verdict": "pass",
                            "candidate_diff_sha256": diff_sha,
                            "test_receipt_sha256": receipt_sha,
                            "findings": findings,
                        },
                        "limitations": limitations,
                    });
                    return Some(WorkflowOutput {
                        collected,
                        receipt,
                        started_at,
                        summary,
                    });
                }
                ReviewVerdict::ChangesRequested => {
                    note(
                        "review_changes_requested",
                        format!(
                            "round={round};findings={};pass_with_blocking={}",
                            eval.findings.len(),
                            eval.pass_with_blocking
                        ),
                    );
                    if round >= max_rounds {
                        return self.workflow_fail(
                            task_id,
                            attempt_no,
                            revision,
                            &format!(
                                "评审退回且已到自动返工上限（{} 次），停止",
                                self.config.max_rework
                            ),
                        );
                    }
                    // ---- 返工：新的实现/验证/评审轮次；原固定基线、同一受管 worktree ----
                    round += 1;
                    let mut envs = self.config.implement_env.clone();
                    envs.insert("IMPLEMENT_ROUND".to_owned(), round.to_string());
                    envs.insert(
                        "REVIEW_REPORT_PATH".to_owned(),
                        review_dir.join("review.json").to_string_lossy().to_string(),
                    );
                    if stop.load(Ordering::SeqCst)
                        || !self.ensure_still_running(task_id, attempt_no)
                    {
                        return None;
                    }
                    let rework = key(round, StageKind::Implement);
                    if let Err(e) = self
                        .store
                        .begin_stage(rework, revision, None, &now_rfc3339())
                    {
                        return fail(format!("第 {round} 轮返工没有阶段启动许可：{e}"));
                    }
                    note(
                        "rework_round",
                        format!("round={round};reason=changes_requested"),
                    );
                    let prepared = self.prepare_managed_implementation(
                        &worktree.path,
                        plan,
                        scope,
                        attempt_id,
                        round,
                        Some(&report_for_rework),
                    );
                    let (result, edits) = match prepared {
                        Ok(Some((command, edits))) => (
                            self.run_tracked_command(
                                command,
                                self.config.claude_implement_fixture.is_some(),
                                task_id,
                                attempt_no,
                                stop,
                            ),
                            Some(edits),
                        ),
                        Ok(None) => (
                            self.run_tracked(
                                &pair.implementer.path,
                                &worktree.path,
                                &envs,
                                task_id,
                                attempt_no,
                                stop,
                            ),
                            None,
                        ),
                        Err(error) => {
                            return fail(format!("第 {round} 轮返工输入准备失败：{error}"))
                        }
                    };
                    let run = match result {
                        Ok(run) => run,
                        Err(failure) => {
                            return self.handle_tracked_failure(
                                task_id,
                                attempt_no,
                                revision,
                                failure,
                                "返工子进程",
                            )
                        }
                    };
                    if let Err(e) = classify_events(&run.events, run.exit_ok) {
                        return self.workflow_fail(
                            task_id,
                            attempt_no,
                            revision,
                            &format!("返工协议不合规：{e}"),
                        );
                    }
                    if let Some(edits) = edits {
                        if stop.load(Ordering::SeqCst) {
                            supervisor.release_after_stop();
                            return None;
                        }
                        if !self.ensure_still_running(task_id, attempt_no) {
                            return None;
                        }
                        let result = run
                            .events
                            .review_verdict
                            .as_ref()
                            .ok_or_else(|| std::io::Error::other("EDIT_SCHEMA"))
                            .and_then(|value| edits.apply(value));
                        if let Err(error) = result {
                            return fail(format!("第 {round} 轮受管返工拒绝：{error}"));
                        }
                    }
                    if let Err(e) = self.store.finish_stage(
                        rework,
                        revision,
                        StageFinish::Implemented,
                        &now_rfc3339(),
                    ) {
                        return fail(format!("第 {round} 轮返工阶段无法提交：{e}"));
                    }
                }
            }
        }
    }
}
