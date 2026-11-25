//! M1 本地 Mock 业务编排（03/04/06/08）：plan → submit → FIFO 领取 →
//! 启动栅栏 → 项目自带 mock 子进程 → 差异与验证收据 → 原子发布并在
//! SQLite 同一事务登记制品引用与状态。
//!
//! 职责与边界：业务契约（04 工具表）经由 [`M1Service`] 分派到 relay-store
//! 的 CAS/幂等/恢复原语与 relay-git 的 Git 层；daemon 通过 Unix socket
//! 会话调用本模块，CLI 与 MCP 共用同一契约。取消只处理本服务确认归属
//! 的进程组（受管子进程自成进程组）；无法证实退出时不报告已取消、
//! 不派下一 attempt。本模块不做真实模型调用、不接外部适配器、不走网络。

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relay_core::{Event, State};
use relay_git::{
    collect_diff, create_worktree, publish_artifact, sha256_hex, CollectedDiff, TestReceipt,
    VerifyConfig, WriteScope,
};
use relay_protocol::{
    ArtifactRequest, CancelRequest, ListRequest, PlanRequest, RetryRequest, ReviewRequest,
    StatusRequest, SubmitRequest,
};
use relay_store::{
    ArtifactRegisterInput, RetryInput, ReviewInput, Store, SubmitInput, TaskSummary,
    TransitionInput,
};
use serde_json::{json, Value};

/// M1 服务配置（临时配置文件；不触碰真实用户配置）。
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub project_binding: Option<ProjectBinding>,
    pub live_routes: Option<crate::live_routes::LiveRoutes>,
    pub opencode_review_fixture: Option<crate::opencode_review::OpenCodeReview>,
    pub opencode_implement_fixture: Option<crate::opencode_review::OpenCodeReview>,
    pub claude_review_fixture: Option<crate::claude_review::LoopbackClaude>,
    pub claude_implement_fixture: Option<crate::claude_review::LoopbackClaude>,
    /// 项目 Git 仓库根（主 checkout，保持不变）。
    pub repo_root: PathBuf,
    /// daemon 私有受管根（worktree 与制品都在其中）。
    pub managed_root: PathBuf,
    /// 项目自带 mock 子进程可执行（只改受管 worktree）。
    pub mock_worker: PathBuf,
    /// 验证规则（M1 本地脚本；真实路线留 M2）。
    pub rules: Vec<relay_git::VerifyRule>,
    /// mock 子进程附加参数（如 "hold"：挂起等待 TERM，取消回归用）。
    pub mock_worker_args: Vec<String>,
    /// 强杀注入点（仅测试）：before_fence / after_fence / after_spawn /
    /// published_before_register；命中后在标记文件存在期间自旋。
    pub stall_at: Option<String>,
    /// 自旋持续期间检查的标记文件；不存在时注入点放行。
    pub stall_file: Option<PathBuf>,
    /// 回收 TERM 预算（毫秒）。
    pub reclaim_term_ms: u64,
    /// 回收 SIGKILL 预算（毫秒）。
    pub reclaim_kill_ms: u64,
    /// 磁盘可用字节下限：低于阈值不领取下一任务（0 = 不启用）。
    pub min_disk_free_bytes: u64,
    /// M2 适配器夹具可执行（05 事件收敛面）：设置后 run 走适配器协议
    /// （probe + worker_* 事件），None 时沿用 mock_worker 直跑。
    pub adapter_fixture: Option<PathBuf>,
    /// 期望的模型身份（05：configured ≠ actual 不合规；None = 不核对）。
    pub expected_model: Option<String>,
    /// 允许的适配器能力清单（05：未知能力拒绝）。
    pub allowed_capabilities: Vec<String>,
    /// probe 等待上限（毫秒）：挂起即超时终止。
    pub adapter_probe_timeout_ms: u64,
    /// run 事件读取等待上限（毫秒）。
    pub adapter_read_timeout_ms: u64,
    /// 适配器单帧字节上限（超出即判为违规帧）。
    pub adapter_max_frame_bytes: usize,
    /// 整组退出确认预算（毫秒）：组长退出后等待整组清空的时限。
    pub group_exit_budget_ms: u64,
    /// mock 子进程总等待上限（毫秒）；超时按 WorkerFailed 处理。
    pub worker_timeout_ms: u64,
    /// 适配器目录（16：空配置下自动推荐的来源）：设置后按角色能力从目录
    /// 中选出实现者与评审者组合（逐项探测）；None 时沿用显式的
    /// adapter_fixture 或 mock_worker。目录与代码分开，不内置任何型号。
    pub adapter_catalog_dir: Option<PathBuf>,
    /// 工作流模板（15）：implement_review（产品默认）或 implementation_only
    /// （诊断/兼容，须显式选择——不静默降级为自评）。
    pub workflow_template: String,
    /// 每个任务最多自动返工次数（15：首版最多一次；0 = 不返工）。
    pub max_rework: u32,
    /// 实现者子进程的环境变量（按角色隔离，不改 daemon 自身环境）。
    pub implement_env: BTreeMap<String, String>,
    /// 评审者子进程的环境变量。
    pub review_env: BTreeMap<String, String>,
}

/// 工作流模板名（15）：实现 → 验证 → 独立评审。
pub const TEMPLATE_IMPLEMENT_REVIEW: &str = "implement_review";
/// 工作流模板名（15）：仅实现（诊断/兼容）。
pub const TEMPLATE_IMPLEMENTATION_ONLY: &str = "implementation_only";

fn parse_env_map(value: &Value, key: &str) -> Result<BTreeMap<String, String>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                let text = v
                    .as_str()
                    .ok_or_else(|| format!("配置 {key}.{k} 必须是字符串"))?;
                out.insert(k.clone(), text.to_owned());
            }
            Ok(out)
        }
        Some(_) => Err(format!("配置 {key} 必须是对象")),
    }
}

const BOUND_PROFILE_PREFIX: &str = "prof_bound_";
/// 03: at most 20 non-terminal tasks may occupy one local instance.
const MAX_NONTERMINAL_TASKS: u64 = 20;
/// 03: 2 GiB managed artifact + worktree quota. Checked periodically before
/// claiming new work; running attempts are allowed to finish and preserve
/// their evidence.
const MAX_MANAGED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Production default: stop claiming new work while the managed volume has
/// less than 512 MiB free. This is a safety floor, not the full 2 GiB
/// artifact/worktree quota; tests may explicitly configure 0.
const DEFAULT_MIN_DISK_FREE_BYTES: u64 = 512 * 1024 * 1024;

/// Bounded recursive size of the managed root. Symlinks are not followed;
/// the walk stops as soon as the quota threshold is exceeded.
fn managed_size(root: &Path, limit: u64) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
                if total > limit {
                    return total;
                }
            }
        }
    }
    total
}

#[derive(Clone, Debug)]
pub struct ProjectBinding {
    project_id: String,
    profile_id: String,
    root: PathBuf,
    device: u64,
    inode: u64,
    git_device: u64,
    git_inode: u64,
    write_scope: WriteScope,
}

impl ProjectBinding {
    fn configure(config: &Value, root: &Path) -> Result<Option<Self>, String> {
        use std::os::unix::fs::MetadataExt;
        let Some(value) = config.get("project_binding") else {
            return Ok(None);
        };
        let map = value.as_object().ok_or("project_binding 必须是对象")?;
        if map.len() != 2 || !map.contains_key("project_id") || !map.contains_key("write_scope") {
            return Err("project_binding 只允许 project_id 和 write_scope".into());
        }
        let id = value["project_id"]
            .as_str()
            .filter(|v| {
                v.starts_with("proj_")
                    && (6..=64).contains(&v.len())
                    && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
            .ok_or("无效的 project_id")?;
        let paths = value["write_scope"]
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or("必须配置非空项目写入范围")?
            .iter()
            .map(|p| p.as_str().ok_or("项目写入范围条目必须是字符串"))
            .collect::<Result<Vec<_>, _>>()?;
        let scope = WriteScope::new(&paths).map_err(|e| e.to_string())?;
        if !root.is_absolute() || root.canonicalize().ok().as_deref() != Some(root) {
            return Err("登记仓库必须是物理绝对路径".into());
        }
        let metadata = std::fs::metadata(root).map_err(|e| e.to_string())?;
        if !metadata.is_dir() {
            return Err("登记仓库必须是目录".into());
        }
        let device = metadata.dev();
        let inode = metadata.ino();
        let git = root.join(".git");
        let git_meta = std::fs::symlink_metadata(&git).map_err(|_| "登记需要 Git 主 checkout")?;
        if !git_meta.is_dir() {
            return Err("登记需要含真实 .git 目录的 Git 主 checkout".into());
        }
        let git_device = git_meta.dev();
        let git_inode = git_meta.ino();
        let fingerprint = serde_json::to_vec(&json!({
            "schema":1, "config":config, "repo_device":device, "repo_inode":inode, "git_device":git_device, "git_inode":git_inode
        }))
        .map_err(|e| e.to_string())?;
        Ok(Some(Self {
            project_id: id.into(),
            profile_id: format!(
                "{BOUND_PROFILE_PREFIX}{}",
                relay_git::sha256_hex(&fingerprint)
            ),
            root: root.to_owned(),
            device,
            inode,
            git_device,
            git_inode,
            write_scope: scope,
        }))
    }

    fn verify_root(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.root.canonicalize().ok().as_ref() == Some(&self.root)
            && std::fs::metadata(&self.root)
                .is_ok_and(|m| m.is_dir() && m.dev() == self.device && m.ino() == self.inode)
            && std::fs::symlink_metadata(self.root.join(".git")).is_ok_and(|m| {
                m.is_dir() && m.dev() == self.git_device && m.ino() == self.git_inode
            })
    }

    fn check(&self, project: &str, profile: &str, requested: &WriteScope) -> Result<(), BizError> {
        if project != self.project_id {
            return Err(BizError::not_found("此实例没有登记所选项目"));
        }
        if profile != self.profile_id || !self.verify_root() {
            return Err(BizError::new(
                "PLAN_STALE",
                "项目配置或仓库身份已改变，请重新查看登记信息",
                "new_plan",
            ));
        }
        let shared = WriteScope::intersection(requested, &self.write_scope);
        if requested.rules().is_empty()
            || !requested.rules().iter().all(|r| shared.rules().contains(r))
        {
            return Err(BizError::invalid("请求的文件范围超出项目授权"));
        }
        Ok(())
    }
}

impl ServiceConfig {
    pub fn attach_live_routes(
        &mut self,
        routes: crate::live_routes::LiveRoutes,
    ) -> Result<(), String> {
        let binding = self
            .project_binding
            .as_mut()
            .ok_or("真实路线要求登记项目")?;
        if self.adapter_catalog_dir.is_some()
            || self.adapter_fixture.is_some()
            || self.claude_implement_fixture.is_some()
            || self.claude_review_fixture.is_some()
            || self.opencode_implement_fixture.is_some()
            || self.opencode_review_fixture.is_some()
            || !self.mock_worker.as_os_str().is_empty()
        {
            return Err("真实路线不能混用离线夹具".into());
        }
        // Credentials are supplied afresh for each launch. A previous queue
        // must not silently execute against this session's accounts.
        let grant = crate::route_pilot::nonce().map_err(str::to_owned)?;
        let profile =
            json!({"project_profile":binding.profile_id,"route":routes.disclosure(),"grant":grant});
        binding.profile_id = format!(
            "{BOUND_PROFILE_PREFIX}{}",
            relay_git::sha256_hex(profile.to_string().as_bytes())
        );
        self.workflow_template = TEMPLATE_IMPLEMENT_REVIEW.into();
        self.max_rework = 0;
        // 10 retries x 30 s (bounded) plus the request itself must fit here.
        self.worker_timeout_ms = 420_000;
        self.adapter_read_timeout_ms = 62_000;
        self.adapter_max_frame_bytes = 128 * 1024;
        self.live_routes = Some(routes);
        Ok(())
    }

    /// 从临时配置 JSON 读取（键：repo_root、managed_root、mock_worker、
    /// rules[{id,executable,argv,timeout_ms}]、worker_timeout_ms）。
    pub fn from_json(value: &Value) -> Result<ServiceConfig, String> {
        let get_path = |key: &str| -> Result<PathBuf, String> {
            let text = value
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("配置缺少 {key}"))?;
            if text.is_empty() {
                return Err(format!("配置 {key} 为空"));
            }
            Ok(PathBuf::from(text))
        };
        let mut rules = Vec::new();
        for rule in value
            .get("rules")
            .and_then(Value::as_array)
            .ok_or_else(|| "配置缺少 rules".to_owned())?
        {
            rules.push(relay_git::VerifyRule {
                id: rule
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "规则缺少 id".to_owned())?
                    .to_owned(),
                executable: rule
                    .get("executable")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "规则缺少 executable".to_owned())?
                    .to_owned(),
                argv: rule
                    .get("argv")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                timeout_ms: rule
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000),
            });
        }
        let repo_root = get_path("repo_root")?;
        let config = ServiceConfig {
            project_binding: ProjectBinding::configure(value, &repo_root)?,
            live_routes: None,
            opencode_review_fixture: crate::opencode_review::OpenCodeReview::from_config(
                value,
                "opencode_review_fixture",
            )?,
            opencode_implement_fixture: crate::opencode_review::OpenCodeReview::from_config(
                value,
                "opencode_implement_fixture",
            )?,
            claude_review_fixture: crate::claude_review::LoopbackClaude::from_config(
                value,
                "claude_review_fixture",
            )?,
            claude_implement_fixture: crate::claude_review::LoopbackClaude::from_config(
                value,
                "claude_implement_fixture",
            )?,
            repo_root,
            managed_root: get_path("managed_root")?,
            // 可选：目录模式下不使用；未配置时 spawn 失败按"从未启动"处置。
            mock_worker: value
                .get("mock_worker")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_default(),
            mock_worker_args: value
                .get("mock_worker_args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            rules,
            stall_at: value
                .get("stall_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            stall_file: value
                .get("stall_file")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            reclaim_term_ms: value
                .get("reclaim_term_ms")
                .and_then(Value::as_u64)
                .unwrap_or(2_000),
            reclaim_kill_ms: value
                .get("reclaim_kill_ms")
                .and_then(Value::as_u64)
                .unwrap_or(2_000),
            min_disk_free_bytes: value
                .get("min_disk_free_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MIN_DISK_FREE_BYTES),
            adapter_fixture: value
                .get("adapter_fixture")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            expected_model: value
                .get("expected_model")
                .and_then(Value::as_str)
                .map(str::to_owned),
            allowed_capabilities: value
                .get("allowed_capabilities")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_else(|| vec!["read_files".to_owned(), "edit_files".to_owned()]),
            adapter_probe_timeout_ms: value
                .get("adapter_probe_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000),
            adapter_read_timeout_ms: value
                .get("adapter_read_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000),
            adapter_max_frame_bytes: value
                .get("adapter_max_frame_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(64 * 1024) as usize,
            group_exit_budget_ms: value
                .get("group_exit_budget_ms")
                .and_then(Value::as_u64)
                .unwrap_or(2_000),
            worker_timeout_ms: value
                .get("worker_timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(120_000),
            adapter_catalog_dir: value
                .get("adapter_catalog_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            workflow_template: value
                .get("workflow_template")
                .and_then(Value::as_str)
                .unwrap_or(TEMPLATE_IMPLEMENT_REVIEW)
                .to_owned(),
            // 返工次数有界（15：首版最多一次；配置值再大也封顶，防无界循环）。
            max_rework: value
                .get("max_rework")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(10) as u32,
            implement_env: parse_env_map(value, "implement_env")?,
            review_env: parse_env_map(value, "review_env")?,
        };
        if config.workflow_template != TEMPLATE_IMPLEMENT_REVIEW
            && config.workflow_template != TEMPLATE_IMPLEMENTATION_ONLY
        {
            return Err(format!(
                "配置 workflow_template 不受支持：{}（可选 {TEMPLATE_IMPLEMENT_REVIEW} 或 {TEMPLATE_IMPLEMENTATION_ONLY}）",
                config.workflow_template
            ));
        }
        if config.opencode_review_fixture.is_some()
            && (config.claude_review_fixture.is_some()
                || config.adapter_catalog_dir.is_none()
                || config.workflow_template != TEMPLATE_IMPLEMENT_REVIEW)
        {
            return Err("OpenCode 合成评审要求自动模板与目录，不能叠加第二评审配置".into());
        }
        if config.claude_implement_fixture.is_some()
            && (config.adapter_catalog_dir.is_none()
                || config.workflow_template != TEMPLATE_IMPLEMENT_REVIEW
                || config.claude_review_fixture.is_some()
                || config.adapter_fixture.is_some())
        {
            return Err("受管实现需要独立目录评审，不能混用相同 Claude 夹具或单实现模式".into());
        }
        if config.opencode_implement_fixture.is_some()
            && (config.adapter_catalog_dir.is_none()
                || config.workflow_template != TEMPLATE_IMPLEMENT_REVIEW
                || config.opencode_review_fixture.is_some()
                || config.claude_implement_fixture.is_some()
                || config.adapter_fixture.is_some())
        {
            return Err(
                "OpenCode 受管实现需独立评审，不能混用 OpenCode 评审、另一实现配置或单实现模式"
                    .into(),
            );
        }
        Ok(config)
    }
}

/// 业务错误（04 稳定错误结构：code/message/action）。
#[derive(Debug, Clone)]
pub struct BizError {
    pub code: &'static str,
    pub message: String,
    pub action: &'static str,
}

impl BizError {
    pub fn new(code: &'static str, message: impl Into<String>, action: &'static str) -> Self {
        BizError {
            code,
            message: message.into(),
            action,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", message, "check_ids")
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message, "fix_request")
    }

    pub fn wrong_state(message: impl Into<String>) -> Self {
        Self::new("WRONG_STATE", message, "refresh_status")
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("SERVICE_UNAVAILABLE", message, "retry_later")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": 1,
            "error": {"code": self.code, "message": self.message, "action": self.action},
        })
    }
}

type ResultT = Result<Value, BizError>;

/// 适配器 stdout 事件读取线程：共享有界读取（达上限即停、等待上限即超时，
/// 管道被后代持有也不无期限阻塞），逐行收敛为事件计数与结论帧。
/// 读完后经 `done` 通知；线程自身有上限，不依赖调用方 join。
pub(crate) fn spawn_event_reader(
    stdout: impl std::io::Read + Send + 'static,
    events: Arc<Mutex<AdapterRunEvents>>,
    done: std::sync::mpsc::Sender<()>,
    max_frame: usize,
    read_budget: Duration,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + read_budget;
        let read = read_bounded(stdout, max_frame, deadline);
        let mut guard = events.lock().expect("adapter events 锁中毒");
        if read.oversized {
            guard.oversized_frames += 1;
            let _ = done.send(());
            return;
        }
        if read.timed_out {
            guard.read_timeouts += 1;
            let _ = done.send(());
            return;
        }
        if read.read_error {
            guard.protocol_failure = Some("OUTPUT_READ_ERROR".into());
            let _ = done.send(());
            return;
        }
        for line in String::from_utf8_lossy(&read.bytes).lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(v) => match v["type"].as_str() {
                    Some("worker_started") => guard.started = true,
                    Some("worker_finished") => {
                        // Stable codes from an explicit wrapped route; never store arbitrary diagnostic text.
                        if let Some(
                            code @ ("MODEL_MISMATCH"
                            | "UNEXPECTED_TOOL_RESPONSE"
                            | "PROVIDER_INCOMPLETE"
                            | "OPENCODE_CONTROL_AUTH"
                            | "OPENCODE_GATEWAY_NO_REQUEST"
                            | "GATEWAY_ROUTE_MISMATCH"
                            | "OPENCODE_SERVER_START"
                            | "OPENCODE_OUTPUT"
                            | "OPENCODE_FIXTURE_TRANSPORT"
                            | "UPSTREAM_AUTH"
                            | "UPSTREAM_RATE_LIMIT"
                            | "UPSTREAM_TRANSPORT"
                            | "UPSTREAM_PROTOCOL"
                            | "UPSTREAM_PROVIDER_ERROR"
                            | "GO_QUOTA_EXHAUSTED"
                            | "GO_USAGE_PROTOCOL"
                            | "GO_BILLING_UNVERIFIED"
                            | "GO_BILLING_NOT_INCLUDED"
                            | "PROJECT_ROUTE_EDIT_PROTOCOL"
                            | "PROJECT_ROUTE_REVIEW_PROTOCOL"),
                        ) = v["diagnostic"].as_str()
                        {
                            guard.protocol_failure = Some(code.to_owned());
                        }
                        let term = v["termination"].as_str().map(str::to_owned);
                        match (&guard.termination, &term) {
                            // 重复终止帧：计数（随后拒绝），不覆盖已确定结果。
                            (Some(first), Some(next)) if first == next => {
                                guard.duplicate_terminals += 1;
                            }
                            // 冲突终止帧：计数，保留首帧结果。
                            (Some(_), Some(_)) => {
                                guard.conflicting_terminals += 1;
                            }
                            _ => guard.termination = term,
                        }
                    }
                    // 评审结论帧：只保留首个，总数计数（结论必须唯一）。
                    Some("review_verdict") => {
                        if guard.review_verdict.is_none() {
                            guard.review_verdict = Some(v["report"].clone());
                            guard.provider_evidence = v.get("provider_evidence").cloned();
                        }
                        guard.review_verdict_count += 1;
                    }
                    // 未知普通事件：计数并跳过（不堆积）。
                    _ => guard.unknown_events += 1,
                },
                Err(_) => guard.invalid_lines += 1,
            }
        }
        drop(guard);
        let _ = done.send(());
    })
}

/// 适配器事件协议分类（05）：成功需要已识别的成功终止帧 + 子进程成功
/// 退出；帧超限、读超时、重复或冲突终止帧均拒绝；认证失败/限流停止
/// 且不切换账号或计费路线；unsupported_protocol 安全结束。
pub(crate) fn classify_events(events: &AdapterRunEvents, exit_ok: bool) -> Result<(), String> {
    if events.oversized_frames > 0 {
        return Err("适配器单帧超过字节上限".to_owned());
    }
    if events.read_timeouts > 0 {
        return Err("适配器事件读取等待超时".to_owned());
    }
    if events.duplicate_terminals > 0 {
        return Err("适配器终止帧重复：同一终止结果被报告多次，拒绝".to_owned());
    }
    if events.conflicting_terminals > 0 {
        return Err("适配器终止帧冲突：已确定结果不被后一帧覆盖".to_owned());
    }
    if let Some(reason) = &events.protocol_failure {
        return Err(reason.clone());
    }
    // 05: a success terminal frame without a recognized worker_started is
    // not a complete adapter lifecycle; never infer success from the end
    // frame alone.
    if !events.started {
        return Err("未收到 worker_started 起始帧".to_owned());
    }
    match events.termination.as_deref() {
        Some("success") if exit_ok => Ok(()),
        Some("auth_failed") => Err("适配器认证失败：停止，不切换账号或计费路线".to_owned()),
        Some("retry_exhausted") => Err("重试上限内仍未成功：无法进行下去，任务失败".to_owned()),
        Some("rate_limited") => Err("适配器限流：停止，不自动转备用路线".to_owned()),
        Some("unsupported_protocol") => Err("适配器报告 unsupported_protocol：安全结束".to_owned()),
        _ => Err("未识别的成功终止帧（收到的帧不足或类型不符）".to_owned()),
    }
}

/// 有界流读取结果（probe 与 run 共用）：字节、是否超限、是否等待超时。
#[derive(Debug, Default)]
pub struct BoundedRead {
    pub bytes: Vec<u8>,
    /// 超过字节上限即停止读取（数据未全部读入）。
    pub oversized: bool,
    /// 等待上限到达（管道仍被后代持有等）。
    pub timed_out: bool,
    /// 读取失败不算完整 EOF，即使已收到合法终止帧。
    pub read_error: bool,
}

/// 有界读取：从流中增量读取，最多 `max_bytes` 字节；到达上限即停止
/// （不把整块数据读进内存），`deadline` 到则报告超时。
///
/// 读取在独立线程中进行，主线程按 deadline 等待——管道被后代持有时
/// 也不无期限阻塞（读线程被放弃，不 join）。
pub(crate) fn read_bounded(
    mut stream: impl std::io::Read + Send + 'static,
    max_bytes: usize,
    deadline: std::time::Instant,
) -> BoundedRead {
    let (tx, rx) = std::sync::mpsc::channel::<BoundedRead>();
    let handle = std::thread::spawn(move || {
        let mut result = BoundedRead::default();
        let mut chunk = [0u8; 4096];
        loop {
            if result.bytes.len() > max_bytes {
                result.oversized = true;
                result.bytes.truncate(max_bytes);
                break;
            }
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => result.bytes.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    result.read_error = true;
                    break;
                }
            }
        }
        let _ = tx.send(result);
    });
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(result) => {
            let _ = handle.is_finished();
            result
        }
        Err(_) => {
            // 等待上限：读线程被放弃（不 join），报告超时。
            BoundedRead {
                timed_out: true,
                ..Default::default()
            }
        }
    }
}

/// probe 失败：区分"进程已确认退出"（worker 从未启动，可解封 gate）
/// 与"回收未确认"（gate 必须保持 pending，持久阻断）。
#[derive(Debug, Clone)]
pub struct ProbeError {
    pub message: String,
    pub group_unconfirmed: bool,
    /// 回收未确认时残留的进程组（供日志与人工核对）。
    pub pgid: Option<i32>,
}

impl ProbeError {
    pub(crate) fn exited(message: impl Into<String>) -> Self {
        ProbeError {
            message: message.into(),
            group_unconfirmed: false,
            pgid: None,
        }
    }

    pub(crate) fn unconfirmed(message: impl Into<String>, pgid: i32) -> Self {
        ProbeError {
            message: message.into(),
            group_unconfirmed: true,
            pgid: Some(pgid),
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 适配器 probe 报告（05：binary identity、version、实际模型、能力）。
#[derive(Debug, Clone, Default)]
pub struct AdapterProbeReport {
    pub binary: String,
    pub version: String,
    pub actual_model: Option<String>,
    pub capabilities: Vec<String>,
    /// 适配器自报的角色（implement / review …）。
    pub roles: Vec<String>,
}

/// 一次 adapter run 的事件汇聚（05 收敛面解析结果）。
#[derive(Debug, Default, Clone)]
pub struct AdapterRunEvents {
    pub started: bool,
    pub protocol_failure: Option<String>,
    /// worker_finished 的 termination（success/auth_failed/…）。
    pub termination: Option<String>,
    /// 未知事件计数（跳过并计数，不堆积）。
    pub unknown_events: usize,
    /// 无法解析的行计数。
    pub invalid_lines: usize,
    /// 单帧超过字节上限的次数。
    pub oversized_frames: usize,
    /// 读取等待超时次数。
    pub read_timeouts: usize,
    /// 重复终止帧次数（不覆盖已确定结果）。
    pub duplicate_terminals: usize,
    /// 冲突终止帧次数（已确定结果不被覆盖）。
    pub conflicting_terminals: usize,
    /// 首个评审结论帧携带的 ReviewReport（只保留首个）。
    pub review_verdict: Option<Value>,
    /// 评审结论帧总数（>1 即不合规：结论必须唯一）。
    pub review_verdict_count: usize,
    pub(crate) provider_evidence: Option<Value>,
}

/// 受管进程组跟踪守卫：pgid 登记后直到 attempt 结束才移除——直接子
/// 进程先退出而组内仍有成员时，取消仍能按 pgid 回收整组（不丢失跟踪）。
struct OwnedGuard {
    owned: Arc<Mutex<HashMap<(String, u32), i32>>>,
    key: (String, u32),
}

impl Drop for OwnedGuard {
    fn drop(&mut self) {
        self.owned.lock().expect("owned 锁中毒").remove(&self.key);
    }
}

/// M1 业务服务：所有方法返回成功结果 JSON 或 [`BizError`]。
pub struct M1Service {
    pub store: Arc<Store>,
    pub config: ServiceConfig,
    /// 受管子进程组（pgid）：取消只处理确认归属的进程组；Arc 供
    /// OwnedGuard 持有（pgid 跟踪到 attempt 结束）。
    pub(crate) owned: Arc<Mutex<HashMap<(String, u32), i32>>>,
    /// 进程控制边界（生产 relay-sys；测试注入替身）。
    pub(crate) group_control: Arc<dyn GroupControl>,
    maintenance: Option<PathBuf>,
    archive: bool,
    inspection: bool,
    admissions: Mutex<()>,
    closing: AtomicBool,
    /// Cached 2 GiB managed-size check: (sample time, within quota).
    quota: Mutex<Option<(Instant, bool)>>,
}

impl M1Service {
    pub fn new(store: Arc<Store>, config: ServiceConfig) -> Arc<M1Service> {
        Self::build(store, config, Arc::new(RealGroupControl), None)
    }

    /// 测试注入进程控制替身。
    pub fn with_group_control(
        store: Arc<Store>,
        config: ServiceConfig,
        group_control: Arc<dyn GroupControl>,
    ) -> Arc<M1Service> {
        Self::build(store, config, group_control, None)
    }

    pub(crate) fn for_instance(
        store: Arc<Store>,
        config: ServiceConfig,
        instance_dir: &Path,
    ) -> Arc<M1Service> {
        Self::build(
            store,
            config,
            Arc::new(RealGroupControl),
            Some(instance_dir.join("upgrade.maintenance")),
        )
    }

    pub(crate) fn for_project_inspection(
        store: Arc<Store>,
        config: ServiceConfig,
        instance_dir: &Path,
    ) -> Arc<M1Service> {
        let mut service = Self::for_instance(store, config, instance_dir);
        Arc::get_mut(&mut service).expect("new service").inspection = true;
        service
    }

    fn in_maintenance(&self) -> bool {
        self.archive || self.maintenance.as_ref().is_some_and(|path| {
            !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        })
    }

    fn build(
        store: Arc<Store>,
        config: ServiceConfig,
        group_control: Arc<dyn GroupControl>,
        maintenance: Option<PathBuf>,
    ) -> Arc<M1Service> {
        let owned = Arc::new(Mutex::new(HashMap::new()));
        let archive = maintenance
            .as_ref()
            .and_then(|path| path.parent())
            .is_some_and(|dir| {
                !matches!(std::fs::symlink_metadata(dir.join("restore.archive")),
                      Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            });
        Arc::new(M1Service {
            store,
            config,
            owned,
            group_control,
            maintenance,
            archive,
            inspection: false,
            admissions: Mutex::new(()),
            closing: AtomicBool::new(false),
            quota: Mutex::new(None),
        })
    }

    pub(crate) fn is_archive(&self) -> bool {
        self.archive
    }

    pub(crate) fn stop_admissions(&self) {
        self.closing.store(true, Ordering::SeqCst);
        // Drain any write already admitted, including accepted sockets whose
        // request arrived while the foreground loop was stopping.
        let _guard = self.admissions.lock().expect("admissions lock");
    }

    /// 04 工具分派：返回业务结果或业务错误 JSON（连接层只做帧与会话）。
    pub fn dispatch(&self, method: &str, params: Option<&Value>) -> Option<Value> {
        let _admission = matches!(
            method,
            "relay_plan" | "relay_submit" | "relay_retry" | "relay_review" | "relay_cancel"
        )
        .then(|| self.admissions.lock().expect("admissions lock"));
        if _admission.is_some() && self.closing.load(Ordering::SeqCst) {
            return Some(
                BizError::new("SERVICE_UNAVAILABLE", "服务正在停止", "retry_after_restart")
                    .to_json(),
            );
        }
        if _admission.is_some() && self.inspection {
            return Some(
                BizError::new(
                    "SERVICE_UNAVAILABLE",
                    "此项目尚未启用执行路线，只能查看",
                    "configure_verified_route",
                )
                .to_json(),
            );
        }
        if _admission.is_some() && self.in_maintenance() {
            return Some(
                BizError::new(
                    "SERVICE_UNAVAILABLE",
                    if self.archive {
                        "恢复档案只读"
                    } else {
                        "应用升级维护中"
                    },
                    if self.archive {
                        "use_active_instance"
                    } else {
                        "retry_after_maintenance"
                    },
                )
                .to_json(),
            );
        }
        let result: ResultT = match method {
            "relay_plan" => self.plan(params),
            "relay_submit" => self.submit(params),
            "relay_status" => self.status(params),
            "relay_list" => self.list(params),
            "relay_artifact" => self.artifact(params),
            "relay_cancel" => self.cancel(params),
            "relay_retry" => self.retry(params),
            "relay_review" => self.review(params),
            "events.list" => self.events(params),
            // 本机管理命名空间（任务窗口的只读总览）；不经 MCP 桥转发。
            "ui.overview" => self.ui_overview(),
            "ui.project" => self.ui_project(),
            _ => Err(BizError::new(
                "NOT_FOUND",
                format!("未知方法 {method}"),
                "check_method",
            )),
        };
        Some(match result {
            Ok(value) => value,
            Err(err) => err.to_json(),
        })
    }

    fn ui_project(&self) -> ResultT {
        let binding = self
            .config
            .project_binding
            .as_ref()
            .ok_or_else(|| BizError::not_found("此实例尚未配置项目登记"))?;
        if !binding.verify_root() || self.archive {
            return Err(BizError::new(
                "SERVICE_UNAVAILABLE",
                "登记的仓库目录不可用",
                "check_project",
            ));
        }
        Ok(json!({"schema_version":1,
            "project_id":binding.project_id,"profile_id":binding.profile_id,
            "repo_root":binding.root,"write_scope":binding.write_scope.rules().iter().map(|r| r.raw()).collect::<Vec<_>>(),
            "verification_rule_ids":self.config.rules.iter().map(|r| &r.id).collect::<Vec<_>>(),
            "workflow":self.config.workflow_template,
            "dispatch_enabled": !self.inspection,
            "external_route":self.config.live_routes.as_ref().map(|r| r.disclosure()),
        }))
    }

    fn check_snapshot(&self, plan: &relay_store::PlanRecord) -> Result<(), BizError> {
        if let Some(binding) = &self.config.project_binding {
            let paths = plan
                .write_scope
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let scope = WriteScope::new(&paths).map_err(|e| BizError::invalid(e.to_string()))?;
            binding.check(&plan.project_id, &plan.profile_id, &scope)
        } else if plan.profile_id.starts_with(BOUND_PROFILE_PREFIX) {
            Err(BizError::new(
                "PLAN_STALE",
                "计划所绑定的项目配置已移除",
                "new_plan",
            ))
        } else {
            Ok(())
        }
    }

    /// Reject a genuinely new submission when the local queue is full, while
    /// still allowing an idempotent replay of an already successful request.
    fn check_queue_capacity(&self, request_id: &str) -> Result<(), BizError> {
        if self
            .store
            .get_request_result(request_id)
            .map_err(store_error_to_biz)?
            .is_some()
        {
            return Ok(());
        }
        let count = self
            .store
            .count_nonterminal_tasks()
            .map_err(store_error_to_biz)?;
        if count >= MAX_NONTERMINAL_TASKS {
            return Err(BizError::new(
                "QUEUE_FULL",
                format!(
                    "本机已有 {count} 个未结束任务（上限 {MAX_NONTERMINAL_TASKS}）；请完成或取消后再提交"
                ),
                "wait_for_capacity",
            ));
        }
        Ok(())
    }

    fn check_new_request_plan(&self, request_id: &str, plan_id: &str) -> Result<(), BizError> {
        // Leave snapshot replay and conflict resolution in the store, even if
        // the registration changed since the first successful submission.
        if self
            .store
            .get_request_result(request_id)
            .map_err(store_error_to_biz)?
            .is_some()
        {
            return Ok(());
        }
        if let Some(plan) = self.store.get_plan(plan_id).map_err(store_error_to_biz)? {
            self.check_snapshot(&plan)?;
        }
        Ok(())
    }

    /// relay_plan：预览并登记计划（无外发；无法授权不返回可提交 plan）。
    fn plan(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            PlanRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        let scope_refs = request
            .write_scope
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let scope = WriteScope::new(&scope_refs).map_err(|e| BizError::invalid(e.to_string()))?;
        if let Some(binding) = &self.config.project_binding {
            binding.check(&request.project_id, &request.profile_id, &scope)?;
        } else if request.profile_id.starts_with(BOUND_PROFILE_PREFIX) {
            return Err(BizError::new("PLAN_STALE", "该项目配置未登记", "new_plan"));
        }
        // 规则必须全部可解析（未知 ID 不允许任何规则启动）。
        let verify_config = VerifyConfig::new(self.config.rules.clone())
            .map_err(|e| BizError::invalid(e.to_string()))?;
        for rule_id in &request.verification_rule_ids {
            verify_config
                .resolve(rule_id)
                .map_err(|e| BizError::invalid(format!("规则 {rule_id} 不可解析：{e}")))?;
        }
        // 基线预检：显式 base_oid 或仓库 HEAD（只读，不改主 checkout）。
        let base_oid = match &request.base_oid {
            Some(oid) => oid.clone(),
            None => relay_git::read_repository_head(&self.config.repo_root)
                .map_err(|e| BizError::invalid(format!("无法读取仓库 HEAD：{e}")))?,
        };
        relay_git::verify_baseline(&self.config.repo_root, &base_oid)
            .map_err(|e| BizError::invalid(format!("基线预检失败：{e}")))?;

        // 16：空配置下按角色能力自动推荐——预览说清各角色将用什么；无可用
        // 组合时明确提示缺什么（每个排除项一个稳定原因），不返回可提交 plan。
        let mut adapters = Value::Null;
        if let Some(routes) = &self.config.live_routes {
            if request.write_scope.is_empty()
                || request.write_scope.len() > 16
                || request.write_scope.iter().any(|p| p.ends_with('/'))
                || request.verification_rule_ids.is_empty()
            {
                return Err(BizError::invalid(
                    "真实流程需指定 1–16 个已有文本文件和验证规则",
                ));
            }
            routes
                .check()
                .map_err(|code| BizError::new("ROUTE_UNAVAILABLE", code, "reapprove_route"))?;
            adapters = routes.selected().describe();
        } else if let Some(dir) = &self.config.adapter_catalog_dir {
            match self.select_pair(dir, None) {
                Ok(pair) => adapters = pair.describe(),
                Err(failure) => {
                    if let Some(pgid) = failure.unconfirmed_pgid {
                        eprintln!(
                            "relay daemon: 预览探测有进程退出未确认（pgid {pgid}），需人工核对"
                        );
                    }
                    return Err(BizError::new(
                        "ROUTE_UNAVAILABLE",
                        format!("无可用角色组合：{}", failure.reasons.join("；")),
                        "check_adapters",
                    ));
                }
            }
        }
        let now = now_rfc3339();
        let plan_id = format!("plan_{}", short_id());
        // 03: a preview is valid for 15 minutes unless a future profile
        // explicitly overrides it.
        let expires_at = plus_seconds(&now, 15 * 60).map_err(BizError::internal)?;
        self.store
            .put_plan(relay_store::PlanInput {
                plan_id: &plan_id,
                project_id: &request.project_id,
                goal: &request.goal,
                brief: &request.brief,
                base_oid: &base_oid,
                created_at: &now,
                expires_at: &expires_at,
                write_scope_json: &serde_json::to_string(&request.write_scope)
                    .map_err(|e| BizError::internal(e.to_string()))?,
                rule_ids_json: &serde_json::to_string(&request.verification_rule_ids)
                    .map_err(|e| BizError::internal(e.to_string()))?,
                profile_id: &request.profile_id,
            })
            .map_err(|e| BizError::invalid(e.to_string()))?;
        Ok(json!({
            "schema_version": 1,
            "plan_id": plan_id,
            "expires_at": expires_at,
            "resolved": {
                "base_oid": base_oid,
                "project_id": request.project_id,
                "profile_id": request.profile_id,
                "write_scope": request.write_scope,
                "rules": request.verification_rule_ids,
                "adapters": adapters,
                "external_route":self.config.live_routes.as_ref().map(|r| r.disclosure()),
                "workflow": self.config.workflow_template,
                "max_rework": self.config.max_rework,
                "catalog": {
                    "version": crate::model_catalog::CATALOG_VERSION,
                    "recommendation": crate::model_catalog::recommend(),
                },
            },
            "warnings": [],
        }))
    }

    /// relay_submit：冻结执行快照，同一成功请求可安全重送。
    fn submit(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            SubmitRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        self.check_new_request_plan(&request.request_id, &request.plan_id)?;
        self.check_queue_capacity(&request.request_id)?;
        let outcome = self
            .store
            .submit(SubmitInput {
                request_id: &request.request_id,
                plan_id: &request.plan_id,
                now: &now_rfc3339(),
            })
            .map_err(store_error_to_biz)?;
        Ok(json!({
            "schema_version": 1,
            "task": {"id": outcome.task_id, "state": outcome.state.to_string(),
                     "revision": outcome.revision, "active_attempt": outcome.active_attempt},
        }))
    }

    /// relay_status：不读取完整日志，不阻塞等待执行。
    fn status(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            StatusRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        self.task_json(&request.task_id)
    }

    fn task_json(&self, task_id: &str) -> ResultT {
        let task = self
            .store
            .get_task(task_id)
            .map_err(|e| BizError::internal(e.to_string()))?
            .ok_or_else(|| BizError::not_found(format!("任务 {task_id} 不存在")))?;
        Ok(self.task_summary_json(&task))
    }

    /// 制品完整性门（06）：磁盘上的完整文件不等于可验收——以实际读取
    /// 的有界字节核对登记摘要、attempt、基线与收据绑定；缺失、篡改或
    /// 链接一律拒绝（ARTIFACT_INVALID）。制品读取与验收共用本门。
    fn verify_registered_artifact(
        &self,
        task_id: &str,
        attempt_no: u32,
    ) -> Result<(relay_store::ArtifactRecord, String), BizError> {
        use std::io::Read;
        let invalid = |detail: String| BizError::new("ARTIFACT_INVALID", detail, "check_artifact");
        let record = self
            .store
            .get_artifact(task_id, attempt_no)
            .map_err(|e| BizError::internal(e.to_string()))?
            .ok_or_else(|| {
                BizError::not_found(format!("attempt {task_id}#{attempt_no} 无已登记制品"))
            })?;
        let base_oid = match self
            .store
            .get_task_plan_id(task_id)
            .map_err(|e| BizError::internal(e.to_string()))?
            .and_then(|pid| self.store.get_plan(&pid).ok().flatten())
        {
            Some(plan) => plan.base_oid,
            None => {
                return Err(invalid(String::from("任务缺少计划快照，无法核对基线绑定")));
            }
        };
        let attempt_id = format!("{task_id}-{attempt_no}");
        let files: [(&str, &str, usize); 4] = [
            ("manifest.json", record.manifest_sha256.as_str(), 64 * 1024),
            ("diff.patch", record.patch_sha256.as_str(), 512 * 1024),
            (
                "test_receipt.json",
                record.receipt_sha256.as_str(),
                64 * 1024,
            ),
            ("handoff.json", record.handoff_sha256.as_str(), 16 * 1024),
        ];
        let mut bytes_map = Vec::new();
        for (name, registered_sha, limit) in files {
            let path = std::path::Path::new(&record.artifact_dir).join(name);
            let meta = fs::symlink_metadata(&path)
                .map_err(|_| invalid(format!("制品文件 {name} 缺失或不可读")))?;
            if meta.is_symlink() {
                return Err(invalid(format!("制品文件 {name} 是符号链接，拒绝")));
            }
            if !meta.is_file() {
                return Err(invalid(format!("制品文件 {name} 不是常规文件")));
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .and_then(|f| f.take(limit as u64 + 1).read_to_end(&mut bytes))
                .map_err(|_| invalid(format!("制品文件 {name} 读取失败")))?;
            if bytes.len() > limit {
                return Err(invalid(format!("制品文件 {name} 超过 {limit} 字节上限")));
            }
            let actual = sha256_hex(&bytes);
            if actual != registered_sha {
                return Err(invalid(format!(
                    "制品文件 {name} 与登记摘要不符：登记 {registered_sha}，实际 {actual}"
                )));
            }
            bytes_map.push((name.to_owned(), bytes));
        }
        // 结构绑定：manifest / 收据 / handoff 都必须指向同一 attempt 与基线。
        let get_bytes = |name: &str| -> &Vec<u8> {
            bytes_map
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b)
                .expect("文件名来自固定表")
        };
        let manifest_value: Value = serde_json::from_slice(get_bytes("manifest.json"))
            .map_err(|_| invalid(String::from("manifest.json 不是合法 JSON")))?;
        if manifest_value["attempt"].as_str() != Some(attempt_id.as_str()) {
            return Err(invalid(String::from("manifest 的 attempt 绑定不符")));
        }
        if manifest_value["base_oid"].as_str() != Some(base_oid.as_str()) {
            return Err(invalid(String::from("manifest 的基线绑定不符")));
        }
        let patch_bytes = get_bytes("diff.patch");
        if manifest_value["patch_sha256"].as_str() != Some(sha256_hex(patch_bytes).as_str()) {
            return Err(invalid(String::from("manifest 声明的补丁摘要与实际不符")));
        }
        let receipt_value: Value = serde_json::from_slice(get_bytes("test_receipt.json"))
            .map_err(|_| invalid(String::from("test_receipt.json 不是合法 JSON")))?;
        if receipt_value["status"].as_str() != Some("pass") {
            return Err(invalid(String::from("落盘收据不是通过状态")));
        }
        if receipt_value["attempt"].as_str() != Some(attempt_id.as_str())
            || receipt_value["base_oid"].as_str() != Some(base_oid.as_str())
        {
            return Err(invalid(String::from("收据的 attempt/基线绑定不符")));
        }
        if receipt_value["diff_sha256"].as_str() != Some(record.patch_sha256.as_str()) {
            return Err(invalid(String::from("收据的被测补丁摘要与登记不符")));
        }
        let handoff_value: Value = serde_json::from_slice(get_bytes("handoff.json"))
            .map_err(|_| invalid(String::from("handoff.json 不是合法 JSON")))?;
        if handoff_value["attempt"].as_str() != Some(attempt_id.as_str())
            || handoff_value["base_oid"].as_str() != Some(base_oid.as_str())
        {
            return Err(invalid(String::from("handoff 的 attempt/基线绑定不符")));
        }
        if handoff_value["diff_sha256"].as_str() != Some(record.patch_sha256.as_str())
            || handoff_value["test_receipt_sha256"].as_str() != Some(record.receipt_sha256.as_str())
        {
            return Err(invalid(String::from("handoff 的制品引用与登记不符")));
        }
        Ok((record, base_oid))
    }

    fn task_summary_json(&self, task: &TaskSummary) -> Value {
        json!({
            "schema_version": 1,
            "task": {
                "id": task.task_id,
                "state": task.state.to_string(),
                "revision": task.revision,
                "active_attempt": task.active_attempt,
            },
            // 04 status contract: clients branch on a stable next action.
            "revision": task.revision,
            "active_attempt": task.active_attempt,
            "next_action": match task.state {
                State::Queued | State::Preparing | State::Launching
                | State::Running | State::Verifying | State::Cancelling => "wait",
                State::AwaitingReview => "review",
                State::Blocked | State::Failed | State::Interrupted => "inspect",
                State::Cancelled | State::Rejected => "retry_or_new_task",
                State::Completed => "none",
            },
        })
    }

    /// relay_list：项目任务分页。
    fn list(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            ListRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        let page = self
            .store
            .list_tasks(
                &request.project_id,
                request.cursor.as_deref(),
                request.limit.unwrap_or(20),
            )
            .map_err(|e| BizError::invalid(e.to_string()))?;
        Ok(json!({
            "schema_version": 1,
            "tasks": page.tasks
                .iter()
                .map(|t| json!({"id": t.task_id, "state": t.state.to_string(),
                                "revision": t.revision, "active_attempt": t.active_attempt}))
                .collect::<Vec<_>>(),
            "next_cursor": page.next_cursor,
        }))
    }

    /// relay_artifact：只按登记引用读取有界切片；未登记不可见。
    fn artifact(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            ArtifactRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        // 完整性门先行：以实际字节核对登记摘要与绑定，篡改/链接/缺失
        // 一律 ARTIFACT_INVALID，不出可读切片。
        let (record, _base) =
            self.verify_registered_artifact(&request.task_id, request.attempt_no)?;
        let file_name = match request.kind.as_str() {
            "manifest" => "manifest.json",
            "diff" => "diff.patch",
            "test_receipt" => "test_receipt.json",
            "handoff" => "handoff.json",
            _ => {
                return Err(BizError::new(
                    "ARTIFACT_UNAVAILABLE",
                    "kind 不受支持",
                    "check_artifact",
                ))
            }
        };
        let offset = request.offset.unwrap_or(0);
        let limit = request.limit.unwrap_or(32 * 1024);
        let path = PathBuf::from(&record.artifact_dir).join(file_name);
        // 有界读取：take(offset+limit) 硬上限，起点须在字符边界。
        use std::io::{Read, Seek};
        let mut file = std::fs::File::open(&path).map_err(|e| {
            BizError::new(
                "ARTIFACT_UNAVAILABLE",
                format!("制品文件不可读：{e}"),
                "check_artifact",
            )
        })?;
        file.seek(std::io::SeekFrom::Start(offset)).map_err(|e| {
            BizError::new(
                "ARTIFACT_UNAVAILABLE",
                format!("制品读取失败：{e}"),
                "check_artifact",
            )
        })?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                BizError::new(
                    "ARTIFACT_UNAVAILABLE",
                    format!("制品读取失败：{e}"),
                    "check_artifact",
                )
            })?;
        if bytes.len() > limit as usize {
            bytes.truncate(limit as usize);
        }
        // 04: slices start at a UTF-8 character boundary. A continuation byte
        // at the requested offset means the client split a character.
        if bytes.first().is_some_and(|byte| byte & 0xC0 == 0x80) {
            return Err(BizError::invalid("offset 必须位于 UTF-8 字符边界"));
        }
        // 末尾向下裁剪到 UTF-8 字符边界（04）。
        while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
            bytes.pop();
        }
        let content = String::from_utf8(bytes).map_err(|e| BizError::internal(e.to_string()))?;
        let next_offset = if content.is_empty() && offset > 0 && request.limit.is_none() {
            None
        } else if offset.saturating_add(content.len() as u64) < self.file_len(&path) {
            Some(offset.saturating_add(content.len() as u64))
        } else {
            None
        };
        Ok(json!({
            "schema_version": 1,
            "task_id": request.task_id,
            "attempt_no": request.attempt_no,
            "kind": request.kind,
            "content": content,
            "next_offset": next_offset,
        }))
    }

    fn file_len(&self, path: &std::path::Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    /// relay_cancel：已活动先回 cancelling；未证实退出不报告已取消。
    fn cancel(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            CancelRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        // 去重在 store 内最先执行（重放返回首次结果，不新增事件）；
        // 非重放的重复取消由状态机拒绝（不产生第二个有效转换）。
        let outcome = self
            .store
            .cancel(relay_store::CancelInput {
                task_id: &request.task_id,
                expected_revision: request.expected_revision,
                request_id: &request.request_id,
                now: &now_rfc3339(),
            })
            .map_err(store_error_to_biz)?;
        // 活动状态进入 cancelling：回收本服务确认归属的进程组。
        if outcome.state == State::Cancelling {
            self.stop_owned_worker(&request.task_id, outcome.active_attempt);
            // 未证实退出时不推进：保持 cancelling 并如实返回。
            let current = self
                .store
                .get_task(&request.task_id)
                .map_err(|e| BizError::internal(e.to_string()))?
                .ok_or_else(|| BizError::not_found("任务消失"))?;
            return Ok(self.task_summary_json(&current));
        }
        Ok(json!({
            "schema_version": 1,
            "task": {"id": outcome.task_id, "state": outcome.state.to_string(),
                     "revision": outcome.revision, "active_attempt": outcome.active_attempt},
        }))
    }

    /// relay_retry：新 plan 同条件；中断未证实退出时 blocked 不创建 attempt。
    fn retry(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            RetryRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        self.check_new_request_plan(&request.request_id, &request.plan_id)?;
        let task = self
            .store
            .get_task(&request.task_id)
            .map_err(|e| BizError::internal(e.to_string()))?
            .ok_or_else(|| BizError::not_found(format!("任务 {} 不存在", request.task_id)))?;
        // A retry from a terminal state creates new work; keep the same
        // non-terminal queue cap. Interrupted/cancelling attempts are already
        // counted and are handled by the evidence checks below.
        if !matches!(task.state, State::Interrupted | State::Cancelling) {
            self.check_queue_capacity(&request.request_id)?;
        }
        // Cancelling always needs the supervisor to settle. Interrupted needs
        // positive evidence that no managed group/gate for this attempt is
        // still pending; otherwise the store keeps the retry blocked.
        let confirmed = match task.state {
            State::Cancelling => false,
            State::Interrupted => self.no_active_worker_evidence(&task)?,
            _ => true,
        };
        let outcome = self
            .store
            .retry(RetryInput {
                task_id: &request.task_id,
                plan_id: &request.plan_id,
                request_id: &request.request_id,
                expected_revision: request.expected_revision,
                no_active_worker_confirmed: confirmed,
                now: &now_rfc3339(),
            })
            .map_err(store_error_to_biz)?;
        Ok(json!({
            "schema_version": 1,
            "task": {"id": outcome.task_id, "state": outcome.state.to_string(),
                     "revision": outcome.revision, "active_attempt": outcome.active_attempt},
        }))
    }

    /// Positive evidence that an interrupted attempt has no managed group or
    /// pending dispatch gate left. A pending gate with an unknown pgid is not
    /// evidence of exit, so it keeps retry blocked until `gate confirm` or a
    /// confirmed group exit resolves it.
    fn no_active_worker_evidence(&self, task: &TaskSummary) -> Result<bool, BizError> {
        let internal = |e: relay_store::StoreError| BizError::internal(e.to_string());
        // Require an explicit resolution record. Merely having no pending row
        // is not enough: a daemon stop can release the gate without a human
        // or trusted-evidence decision, and the task is still interrupted.
        if self
            .store
            .get_gate_resolution(&task.task_id, task.active_attempt)
            .map_err(internal)?
            .is_none()
        {
            return Ok(false);
        }
        let gates = self.store.list_pending_gates().map_err(internal)?;
        if gates
            .iter()
            .any(|gate| gate.task_id == task.task_id && gate.attempt_no == task.active_attempt)
        {
            return Ok(false);
        }
        let groups = self.store.list_managed_groups(None).map_err(internal)?;
        if groups.iter().any(|group| {
            group.task_id == task.task_id
                && group.attempt_no == task.active_attempt
                && matches!(group.state.as_str(), "tracking" | "blocking")
        }) {
            return Ok(false);
        }
        Ok(true)
    }

    /// relay_review：制品完整性门（磁盘字节核对登记摘要/attempt/基线/
    /// 收据绑定）先行——篡改或缺失一律拒绝且状态不变；随后 store.review
    /// 做 awaiting_review/CAS/摘要门与 request_id 幂等（重放返回首次
    /// 结果、不新增事件）。
    fn review(&self, params: Option<&Value>) -> ResultT {
        let params = params.ok_or_else(|| BizError::invalid(String::from("缺少 params")))?;
        let request =
            ReviewRequest::from_params(params).map_err(|e| BizError::invalid(e.to_string()))?;
        self.verify_registered_artifact(&request.task_id, request.attempt_no)?;
        let outcome = self
            .store
            .review(ReviewInput {
                task_id: &request.task_id,
                attempt_no: request.attempt_no,
                expected_revision: request.expected_revision,
                request_id: &request.request_id,
                verdict: &request.verdict,
                diff_sha256: &request.diff_sha256,
                test_sha256: &request.test_sha256,
                note: request.note.as_deref().unwrap_or(""),
                now: &now_rfc3339(),
            })
            .map_err(store_error_to_biz)?;
        Ok(json!({
            "schema_version": 1,
            "task": {"id": outcome.task_id, "state": outcome.state.to_string(),
                     "revision": outcome.revision, "active_attempt": outcome.active_attempt},
        }))
    }

    /// 任务窗口的只读总览（07）：服务状态与版本、派发门、最近任务；待
    /// 验收任务附交付里的评审结论（经制品完整性门读取 handoff）。
    fn ui_overview(&self) -> ResultT {
        let _admission = self.admissions.lock().expect("admissions lock");
        let internal = |e: relay_store::StoreError| BizError::internal(e.to_string());
        let pending = self.store.list_pending_gates().map_err(internal)?.len();
        let blocking = self
            .store
            .list_managed_groups(Some("blocking"))
            .map_err(internal)?
            .len();
        let tasks = self.store.list_recent_tasks(50).map_err(internal)?;
        let mut items = Vec::with_capacity(tasks.len());
        for task in &tasks {
            let mut review = Value::Null;
            if task.state == State::AwaitingReview {
                review = match self.verify_registered_artifact(&task.task_id, task.active_attempt) {
                    Ok((record, _)) => {
                        let handoff = std::fs::read_to_string(
                            Path::new(&record.artifact_dir).join("handoff.json"),
                        )
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                        match handoff.and_then(|h| h.get("workflow").cloned()) {
                            Some(workflow) => json!({
                                "verdict": workflow["review"]["verdict"],
                                "rounds": workflow["rounds"],
                                "reworks": workflow["reworks"],
                                "candidate_diff_sha256": workflow["review"]["candidate_diff_sha256"],
                                "test_receipt_sha256": workflow["review"]["test_receipt_sha256"],
                                "implement": workflow["routing"]["implement"],
                                "reviewer": workflow["routing"]["review"],
                                "findings": workflow["review"]["findings"],
                            }),
                            None => json!({"error": "ARTIFACT_UNAVAILABLE"}),
                        }
                    }
                    // 磁盘上的候选与登记不符：如实显示，不当作可验收。
                    Err(err) => json!({"error": err.code, "detail": err.message}),
                };
            }
            let stages = self
                .store
                .list_stage_runs(&task.task_id, task.active_attempt)
                .unwrap_or_default()
                .into_iter()
                .map(|stage: relay_store::StageRunRecord| {
                    json!({
                        "round": stage.round,
                        "stage": stage.stage,
                        "state": stage.state,
                        "started_at": stage.started_at,
                        "ended_at": stage.ended_at,
                        "diff_sha256": stage.diff_sha256,
                        "receipt_sha256": stage.receipt_sha256,
                    })
                })
                .collect::<Vec<_>>();
            items.push(json!({
                "id": task.task_id,
                "project_id": task.project_id,
                "state": task.state.to_string(),
                "revision": task.revision,
                "active_attempt": task.active_attempt,
                "goal": task.goal,
                "reason": task.reason_code,
                "created_at": task.created_at,
                "updated_at": task.updated_at,
                "review": review,
                "stages": stages,
            }));
        }
        Ok(json!({
            "schema_version": 1,
            "service": {
                // 能应答即 ready；派发门有待确认/阻断时为 degraded。
                "state": if self.archive { "archive" } else if self.in_maintenance() { "maintenance" } else if pending > 0 || blocking > 0 { "degraded" } else { "ready" },
                "dispatch_gate": {"pending": pending, "blocking": blocking},
                "versions": {
                    "product": env!("CARGO_PKG_VERSION"),
                    "business": "relay.v1",
                    "wire": 1,
                    "db_schema": relay_store::SCHEMA_VERSION,
                },
            },
            "tasks": items,
        }))
    }

    /// events.list（04：M1 CLI 短轮询）。
    fn events(&self, params: Option<&Value>) -> ResultT {
        let empty = json!({});
        let params = params.unwrap_or(&empty);
        let request = relay_protocol::EventsRequest::from_params(params)
            .map_err(|e| BizError::invalid(e.to_string()))?;
        let events = self
            .store
            .list_events(request.after_seq.unwrap_or(0), request.limit.unwrap_or(50))
            .map_err(|e| BizError::invalid(e.to_string()))?;
        Ok(json!({
            "schema_version": 1,
            "events": events
                .iter()
                .map(|e| json!({"seq": e.seq, "task_id": e.task_id, "attempt_no": e.attempt_no,
                                "revision": e.revision, "kind": e.kind, "state": e.state,
                                "reason_code": e.reason_code, "created_at": e.created_at}))
                .collect::<Vec<_>>(),
        }))
    }

    /// 强杀注入点（08 T08 的可重复等价物）：命中点在标记文件存在期间
    /// 自旋，供外部强杀临时 daemon；停止标志或标记移除后放行。
    fn stall(&self, point: &str, task_id: &str, stop: &AtomicBool) -> bool {
        if self.config.stall_at.as_deref() != Some(point) {
            return false;
        }
        while !stop.load(Ordering::SeqCst) {
            // 任务状态离开当前阶段（取消/中断/失败）即放行，worker 不卡死。
            if let Ok(Some(t)) = self.store.get_task(task_id) {
                let text = t.state.to_string();
                if text == "cancelling"
                    || text == "cancelled"
                    || text == "interrupted"
                    || text == "failed"
                {
                    break;
                }
            }
            match &self.config.stall_file {
                Some(marker) if !marker.exists() => break,
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        stop.load(Ordering::SeqCst)
    }

    pub(crate) fn prepare_managed_implementation(
        &self,
        worktree: &Path,
        plan: &relay_store::PlanRecord,
        scope: &WriteScope,
        attempt_id: &str,
        round: u32,
        feedback: Option<&Value>,
    ) -> std::io::Result<Option<(Command, crate::managed_edits::EditRequest)>> {
        let claude = self.config.claude_implement_fixture.as_ref();
        let opencode = self.config.opencode_implement_fixture.as_ref();
        let live = self.config.live_routes.as_ref();
        if claude.is_none() && opencode.is_none() && live.is_none() {
            return Ok(None);
        }
        let edits = crate::managed_edits::EditRequest::new(worktree, plan, scope, feedback)?;
        if let Some(routes) = live {
            let command = routes.command(
                crate::live_routes::Stage::Implement,
                &self.config.repo_root,
                &self.config.managed_root,
                &edits.payload,
            )?;
            return Ok(Some((command, edits)));
        }
        let private =
            crate::claude_review::private_input_dir(&self.config.managed_root, attempt_id, round)?;
        let command = if let Some(fixture) = claude {
            fixture.implement_command(
                &private,
                &self.config.repo_root,
                &self.config.managed_root,
                &edits.payload,
            )?
        } else {
            opencode
                .expect("checked managed route")
                .command_with_payload(
                    &private,
                    &self.config.repo_root,
                    &self.config.managed_root,
                    &edits.payload,
                    true,
                )?
        };
        Ok(Some((command, edits)))
    }

    /// worker 循环体：FIFO 领取一次并执行完整 attempt。串行单线程。
    /// 领取前做磁盘门：受管根可用空间低于阈值时不领取（任务保持
    /// queued，不误报失败），下个轮询重查。
    /// 整组退出确认（统一监管）：只有确认进程组为空才是可信退出证据。
    /// 探测出错或仍有成员 → false（不解封，持久阻断）。
    fn confirm_group_exit(&self, pgid: i32, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            match self.group_control.probe(pgid) {
                Ok(false) => return true,
                Ok(true) | Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 组长退出后的统一收口：确认整组退出才可继续；未确认则持久阻断
    /// （gate pending）+ 任务失败（不产生可验收候选）。
    ///
    /// 返回 true 表示整组已确认退出、可继续后续判定。`release_gate` 为
    /// false 时确认退出后不解封（后续还有受派发门约束的子进程）。
    fn settle_after_leader_exit(
        &self,
        task_id: &str,
        attempt_no: u32,
        expected_revision: u64,
        pgid: i32,
        reason_unconfirmed: &str,
        release_gate: bool,
    ) -> bool {
        let budget = Duration::from_millis(self.config.group_exit_budget_ms);
        // A leader exit is not a group exit. If the group still has members,
        // reclaim it (TERM -> SIGKILL with bounded budgets) instead of leaving
        // descendants alive behind a permanent dispatch block.
        let confirmed = self.confirm_group_exit(pgid, budget)
            || self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
        if confirmed {
            let _ = self
                .store
                .resolve_managed_group(task_id, attempt_no, &now_rfc3339());
            // 自动链路激活时 gate 继续持有（评审/返工子进程仍受派发门
            // 约束），由调用方在轮次结束后解封。
            if release_gate {
                let _ = self.store.release_dispatch_gate(
                    task_id,
                    attempt_no,
                    "整组退出已确认",
                    &now_rfc3339(),
                );
            }
            true
        } else {
            // 未能确认整组退出：持久阻断（gate pending），任务失败——
            // B 保持 queued，重启也不派发。
            let _ =
                self.store
                    .set_gate_reason(task_id, attempt_no, reason_unconfirmed, &now_rfc3339());
            self.fail_worker(task_id, attempt_no, expected_revision, reason_unconfirmed);
            false
        }
    }

    /// 受管恢复操作（缺进程记录时的受管解封）：没有可信证据就保持
    /// 阻断；有证据时记录 attempt、证据与结果（审计）。
    ///
    /// - `evidence` 为空 → 拒绝（保持阻断）。
    /// - 组记录存在且探测确认组已空 → 可信证据（confirmed-exit）。
    /// - 组记录缺失或探测出错 → 仅接受运维明确确认（operator-confirmed
    ///   + 必填说明）；探测显示组仍存活 → 拒绝（证据矛盾）。
    pub fn confirm_gate_absence(
        &self,
        task_id: &str,
        attempt_no: u32,
        operator: &str,
        note: &str,
        operator_confirmed: bool,
    ) -> Result<relay_store::GateResolution, BizError> {
        if note.trim().is_empty() {
            return Err(BizError::invalid("证据说明不能为空"));
        }
        let now = now_rfc3339();
        let groups = self
            .store
            .list_managed_groups(None)
            .map_err(|e| BizError::internal(e.to_string()))?;
        let group = groups
            .into_iter()
            .find(|g| g.task_id == task_id && g.attempt_no == attempt_no);
        // 无组记录时回退到待确认标记上登记的进程组（probe 等）。
        let group = match group {
            Some(g) => Some(g),
            None => self
                .store
                .list_pending_gates()
                .ok()
                .and_then(|gates| {
                    gates
                        .into_iter()
                        .find(|g| g.task_id == task_id && g.attempt_no == attempt_no && g.pgid > 0)
                })
                .map(|g| relay_store::ManagedGroupRecord {
                    task_id: g.task_id,
                    attempt_no: g.attempt_no,
                    pgid: g.pgid,
                    leader_pid: 0,
                    leader_start: String::new(),
                    state: g.state,
                    reason: g.reason,
                    created_at: g.created_at,
                    updated_at: g.updated_at,
                }),
        };
        let (evidence, result) = match &group {
            Some(g) => match self.group_control.probe(g.pgid) {
                Ok(false) => (
                    format!("进程组 {} 探测为空（确认退出）", g.pgid),
                    "confirmed-exit",
                ),
                Ok(true) => {
                    return Err(BizError::new(
                        "SERVICE_UNAVAILABLE",
                        format!("进程组 {} 仍有存活成员，证据不足，保持阻断", g.pgid),
                        "wait_or_recheck",
                    ))
                }
                Err(e) => {
                    if !operator_confirmed {
                        return Err(BizError::new(
                            "SERVICE_UNAVAILABLE",
                            format!("进程组 {} 探测失败（{e}）：需运维确认后解封", g.pgid),
                            "confirm_manually",
                        ));
                    }
                    (
                        format!("进程组 {} 探测失败（{e}）；运维确认：{note}", g.pgid),
                        "operator-confirmed",
                    )
                }
            },
            None => {
                if !operator_confirmed {
                    return Err(BizError::new(
                        "SERVICE_UNAVAILABLE",
                        "缺进程记录：需运维确认进程不存在后解封",
                        "confirm_manually",
                    ));
                }
                (
                    format!("缺进程记录；运维确认：{note}"),
                    "operator-confirmed",
                )
            }
        };
        self.store
            .resolve_gate_with_evidence(task_id, attempt_no, &evidence, result, operator, &now)
            .map_err(|e| BizError::internal(e.to_string()))
    }

    /// 派发门解除：探测可信退出证据（组为空）才释放 gate 标记。缺进程
    /// 记录（pgid 未知）、探测出错或仍有成员——一律保持阻断。
    fn resolve_expired_blocks(&self) {
        let now = now_rfc3339();
        // 同一 attempt 的待确认标记上若还挂着别的受控进程组（探测/评审/
        // 返工子进程），它也必须确认退出，整个 attempt 才算可信退出。
        let gate_group_gone = |task_id: &str, attempt_no: u32| -> bool {
            let gates = match self.store.list_pending_gates() {
                Ok(rows) => rows,
                Err(_) => return false,
            };
            let Some(gate) = gates
                .into_iter()
                .find(|g| g.task_id == task_id && g.attempt_no == attempt_no)
            else {
                // 已无待确认标记（已解封）：不构成阻碍。
                return true;
            };
            if gate.pgid <= 0 {
                return true;
            }
            if !matches!(self.group_control.probe(gate.pgid), Ok(false)) {
                eprintln!(
                    "relay daemon: attempt {task_id}#{attempt_no} 的待确认标记仍挂有进程组 {}（未确认退出），保持阻断",
                    gate.pgid
                );
                return false;
            }
            let _ = self.store.set_gate_pgid(task_id, attempt_no, 0, &now);
            true
        };
        let settle_cancelling = |task_id: &str| {
            if let Ok(Some(t)) = self.store.get_task(task_id) {
                if t.state == State::Cancelling {
                    let _ = self.store.transition(TransitionInput {
                        task_id,
                        expected_active_attempt: t.active_attempt,
                        expected_revision: t.revision,
                        event: Event::AllOwnedWorkersStopped,
                        now: &now,
                    });
                }
            }
        };
        // 受管组路径：组为空且标记上无其他存活进程组 → 解封。
        let rows = match self.store.list_managed_groups(Some("blocking")) {
            Ok(rows) => rows,
            Err(_) => return,
        };
        for row in rows {
            if !matches!(self.group_control.probe(row.pgid), Ok(false)) {
                // 探测出错或仍有成员：保持阻断（不自动解封）。
                continue;
            }
            if !gate_group_gone(&row.task_id, row.attempt_no) {
                continue;
            }
            if self
                .store
                .resolve_managed_group(&row.task_id, row.attempt_no, &now)
                .is_err()
            {
                continue;
            }
            // 可信退出证据：解除 gate 标记（派发门放行）。
            let _ = self.store.release_dispatch_gate(
                &row.task_id,
                row.attempt_no,
                "组退出已证实",
                &now,
            );
            eprintln!(
                "relay daemon: 组 {pgid}（{task}#{n}）退出已证实，解除派发门",
                pgid = row.pgid,
                task = row.task_id,
                n = row.attempt_no
            );
            settle_cancelling(&row.task_id);
        }
        // 带 pgid 的待确认标记（探测/评审/返工等未进入 managed_groups 的
        // 子进程）：探测确认组为空才是可信退出证据 → 带证据解封并写审计；
        // 探测出错或仍有成员一律保持阻断。
        let gates = match self.store.list_pending_gates() {
            Ok(rows) => rows,
            Err(_) => return,
        };
        for gate in gates.iter().filter(|g| g.pgid > 0) {
            if !matches!(self.group_control.probe(gate.pgid), Ok(false)) {
                continue;
            }
            match self.store.resolve_gate_with_evidence(
                &gate.task_id,
                gate.attempt_no,
                &format!("进程组 {} 探测为空（确认退出）", gate.pgid),
                "confirmed-exit",
                "daemon",
                &now,
            ) {
                Ok(record) => {
                    eprintln!(
                        "relay daemon: 待确认标记 {}#{} 退出已证实（{}），解除派发门",
                        gate.task_id, gate.attempt_no, record.evidence
                    );
                    settle_cancelling(&gate.task_id);
                }
                Err(e) => eprintln!(
                    "relay daemon: 待确认标记 {}#{} 解封失败：{e}",
                    gate.task_id, gate.attempt_no
                ),
            }
        }
        // pgid 未知的待确认标记（spawn 前中断或登记缺失）：无可信证据，
        // 保持阻断——需人工经 `relayctl gate confirm` 确认。
    }

    /// 2 GiB managed artifact + worktree quota, sampled at most every five
    /// seconds. Exceeding it keeps tasks queued; running attempts are allowed
    /// to finish so they can preserve their evidence.
    fn quota_ok(&self) -> bool {
        let now = Instant::now();
        let mut cached = self.quota.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, ok)) = *cached {
            if now.duration_since(at) < Duration::from_secs(5) {
                return ok;
            }
        }
        let size = managed_size(&self.config.managed_root, MAX_MANAGED_BYTES);
        let ok = size <= MAX_MANAGED_BYTES;
        if !ok {
            eprintln!("relay daemon: 受管制品/工作树 {size} 字节超过 2 GiB 配额，暂停领取新任务");
        }
        *cached = Some((now, ok));
        ok
    }

    pub fn run_one_claim(&self, stop: &AtomicBool) -> bool {
        if self.inspection || stop.load(Ordering::SeqCst) {
            return false;
        }
        self.resolve_expired_blocks();
        if !self.quota_ok() {
            return false;
        }
        if self.config.min_disk_free_bytes > 0 {
            match relay_sys::free_space(&self.config.managed_root) {
                Ok(free) if free < self.config.min_disk_free_bytes => {
                    eprintln!(
                        "relay daemon: 磁盘可用 {free} 字节低于下限 {}，暂停领取",
                        self.config.min_disk_free_bytes
                    );
                    return false;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("relay daemon: 磁盘空间检查失败：{e}，本轮不领取");
                    return false;
                }
            }
        }
        let admission = self.admissions.lock().expect("admissions lock");
        if stop.load(Ordering::SeqCst) || self.in_maintenance() {
            return false;
        }
        let now = now_rfc3339();
        let claimed = match self.store.claim_next(&now) {
            Ok(Some(outcome)) => outcome,
            Ok(None) => return false,
            Err(_) => return false,
        };
        drop(admission);
        self.run_attempt(&claimed.task_id, claimed.active_attempt, stop);
        true
    }

    /// 单个 attempt 全流程：worktree → 栅栏 → mock 子进程 → 差异 →
    /// 收据 → 发布 → 同事务登记。任何失败都转为对应失败事件。
    fn run_attempt(&self, task_id: &str, attempt_no: u32, stop: &AtomicBool) {
        let attempt_id = format!("{task_id}-{attempt_no}");
        let now = || now_rfc3339();
        // 1. 冻结快照：plan 的 scope/rules/base_oid。
        let task = match self.store.get_task(task_id) {
            Ok(Some(t)) => t,
            _ => return,
        };
        let plan_id = match self.store.get_task_plan_id(task_id) {
            Ok(Some(p)) => p,
            _ => {
                self.fail_preparing(task_id, attempt_no, task.revision, "任务缺少计划引用");
                return;
            }
        };
        let plan = match self.store.get_plan(&plan_id) {
            Ok(Some(p)) => p,
            _ => {
                self.fail_preparing(task_id, attempt_no, task.revision, "计划快照缺失");
                return;
            }
        };
        if task.state != State::Preparing {
            // 领取后已被取消等竞态：不再推进。
            return;
        }
        if let Err(err) = self.check_snapshot(&plan) {
            self.fail_preparing(task_id, attempt_no, task.revision, &err.message);
            return;
        }
        let scope_refs: Vec<&str> = plan.write_scope.iter().map(String::as_str).collect();
        let scope = match WriteScope::new(&scope_refs) {
            Ok(s) => s,
            Err(e) => {
                self.fail_preparing(
                    task_id,
                    attempt_no,
                    task.revision,
                    &format!("写入范围解析失败：{e}"),
                );
                return;
            }
        };
        let verify_config = match VerifyConfig::new(self.config.rules.clone()) {
            Ok(c) => c,
            Err(e) => {
                self.fail_preparing(
                    task_id,
                    attempt_no,
                    task.revision,
                    &format!("规则配置失败：{e}"),
                );
                return;
            }
        };
        // 2. 受管 worktree（preparing）。
        let wt = match create_worktree(
            &self.config.repo_root,
            &self.config.managed_root,
            task_id,
            attempt_no,
            &plan.base_oid,
        ) {
            Ok(r) => r,
            Err(e) => {
                self.fail_preparing(
                    task_id,
                    attempt_no,
                    task.revision,
                    &format!("worktree 创建失败：{e}"),
                );
                return;
            }
        };
        if self.stall("before_fence", task_id, stop) {
            return;
        }
        // 3. 启动栅栏（preparing → launching，持久化许可后才 spawn）。
        let launch_code = format!("launch-{}-{}", attempt_id, short_id());
        if let Err(_e) = self.store.launch_fence(relay_store::LaunchFenceInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision: task.revision,
            launch_code: &launch_code,
            worktree_id: &attempt_id,
            now: &now(),
        }) {
            // 栅栏失败（竞态取消/CAS 过期）：不 spawn，直接按当前状态处置。
            return;
        }
        if self.stall("after_fence", task_id, stop) {
            return;
        }
        // 3.5 适配器 probe（05：身份与能力核对）。probe 可启动子进程，
        //     因此受持久派发门约束：只在启动许可（gate pending）提交后
        //     运行。probe 超时且回收未确认 → gate 保持 pending（持久阻断，
        //     B 保持 queued，重启也不派发）；probe 拒绝（身份/能力不合规）
        //     属"从未启动 worker"的可信证据 → 解封 gate 并 blocked。
        if let Some(fixture) = self.config.adapter_fixture.clone() {
            match self.adapter_probe(&fixture, Some((task_id, attempt_no))) {
                Ok(report) => {
                    let mut rejects = Vec::new();
                    if let (Some(expected), Some(actual)) =
                        (&self.config.expected_model, &report.actual_model)
                    {
                        if actual != expected {
                            rejects.push(format!("模型身份不匹配：配置 {expected}，实际 {actual}"));
                        }
                    }
                    if report.actual_model.is_none() && self.config.expected_model.is_some() {
                        rejects.push("模型身份未核实；不当匹配对待".to_owned());
                    }
                    for cap in &report.capabilities {
                        if !self.config.allowed_capabilities.contains(cap) {
                            rejects.push(format!("未授权能力：{cap}"));
                        }
                    }
                    if !rejects.is_empty() {
                        eprintln!(
                            "relay daemon: attempt {task_id}#{attempt_no} 适配器 probe 拒绝：{rejects:?}"
                        );
                        // 明确未启动：worker 从未启动 → 失败状态、事件、
                        // 解封与审计同一事务提交。
                        if let Err(commit_err) = self.store.commit_spawn_not_started(
                            task_id,
                            attempt_no,
                            task.revision + 1,
                            &format!("probe 拒绝，worker 从未启动：{rejects:?}"),
                            &now(),
                        ) {
                            eprintln!(
                                "relay daemon: attempt {task_id}#{attempt_no} 未启动提交失败（已整体回滚，保持阻断）：{commit_err}"
                            );
                        }
                        return;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "relay daemon: attempt {task_id}#{attempt_no} 适配器 probe 失败：{}（回收未确认：{}）",
                        e.message, e.group_unconfirmed
                    );
                    if e.group_unconfirmed {
                        // 回收无法确认：gate 保持 pending（持久阻断）。
                        let _ = self.store.set_gate_reason(
                            task_id,
                            attempt_no,
                            &format!("probe 失败且回收未确认：{}", e.message),
                            &now(),
                        );
                        let _ = self.store.transition(TransitionInput {
                            task_id,
                            expected_active_attempt: attempt_no,
                            expected_revision: task.revision + 1,
                            event: Event::MarkInterrupted,
                            now: &now(),
                        });
                    } else {
                        // probe 自身失败但进程已确认退出：worker 从未启动
                        // → 失败状态、事件、解封与审计同一事务提交。
                        if let Err(commit_err) = self.store.commit_spawn_not_started(
                            task_id,
                            attempt_no,
                            task.revision + 1,
                            &format!("probe 失败但进程已确认退出，worker 从未启动：{}", e.message),
                            &now(),
                        ) {
                            eprintln!(
                                "relay daemon: attempt {task_id}#{attempt_no} 未启动提交失败（已整体回滚，保持阻断）：{commit_err}"
                            );
                        }
                    }
                    return;
                }
            }
        }
        // 3.6 目录模式（16）：空配置下按角色能力从目录选出实现者 + 独立
        //     评审组合（逐项探测，受派发门约束）。探测是"可启动子进程"
        //     的操作：任一 probe 退出未确认即停止后续探测，按退出不明处置
        //     （gate 保持 pending 并记下该进程组）。无可用组合 = worker
        //     从未启动 → 失败状态、事件、解封与审计同一事务提交。
        let selection: Option<crate::workflow::SelectedPair> = if let Some(routes) =
            &self.config.live_routes
        {
            Some(routes.selected())
        } else if let Some(dir) = self.config.adapter_catalog_dir.clone() {
            match self.select_pair(&dir, Some((task_id, attempt_no))) {
                Ok(pair) => Some(pair),
                Err(failure) => {
                    let detail = failure.reasons.join("；");
                    eprintln!(
                        "relay daemon: attempt {task_id}#{attempt_no} 无可用角色组合：{detail}"
                    );
                    if let Some(pgid) = failure.unconfirmed_pgid {
                        // 退出不明：进程号留在待确认标记上，持久阻断。
                        let _ = self.store.set_gate_pgid(task_id, attempt_no, pgid, &now());
                        let _ = self.store.set_gate_reason(
                            task_id,
                            attempt_no,
                            &format!("目录探测退出未确认（pgid {pgid}）：{detail}"),
                            &now(),
                        );
                        let _ = self.store.transition(TransitionInput {
                            task_id,
                            expected_active_attempt: attempt_no,
                            expected_revision: task.revision + 1,
                            event: Event::MarkInterrupted,
                            now: &now(),
                        });
                        return;
                    }
                    if let Err(commit_err) = self.store.commit_spawn_not_started(
                        task_id,
                        attempt_no,
                        task.revision + 1,
                        &format!("无可用角色组合，worker 从未启动：{detail}"),
                        &now(),
                    ) {
                        eprintln!(
                                "relay daemon: attempt {task_id}#{attempt_no} 未启动提交失败（已整体回滚，保持阻断）：{commit_err}"
                            );
                    }
                    return;
                }
            }
        } else {
            None
        };
        // 自动链路（15）：选出了独立评审者时，实现 → 验证 → 评审的轮次在
        // running 态内完成；gate 持有到轮次结束。
        let review_active = selection
            .as_ref()
            .is_some_and(|pair| pair.reviewer.is_some());
        let first_stage = relay_store::StageKey {
            task_id,
            attempt_no,
            round: 1,
            kind: relay_store::StageKind::Implement,
        };
        if review_active {
            if let Err(e) = self
                .store
                .begin_stage(first_stage, task.revision + 1, None, &now())
            {
                eprintln!("relay daemon: 首轮实现未获得阶段启动许可：{e}");
                if matches!(self.store.get_task(task_id), Ok(Some(t)) if
                    t.active_attempt == attempt_no && matches!(t.state, State::Cancelling | State::Cancelled))
                    && matches!(self.store.has_active_stage(task_id, attempt_no), Ok(false))
                {
                    self.confirm_workers_stopped(task_id, attempt_no);
                    let _ = self.store.release_dispatch_gate(
                        task_id,
                        attempt_no,
                        "启动许可前已取消",
                        &now(),
                    );
                }
                return;
            }
        }
        // 4. spawn：目录选出的实现者、显式适配器夹具（M2 事件收敛面，
        // stdout 解析事件）或普通 mock worker。spawn 时即自成进程组，避免
        // 父侧 setpgid 与 exec 的竞态；launching → running。
        let claude = self.config.claude_implement_fixture.is_some();
        let managed = claude
            || self.config.opencode_implement_fixture.is_some()
            || self.config.live_routes.is_some();
        let prepared =
            self.prepare_managed_implementation(&wt.path, &plan, &scope, &attempt_id, 1, None);
        let (managed_cmd, edit_request) = match prepared {
            Ok(Some((cmd, edits))) => (Some(cmd), Some(edits)),
            Ok(None) => (None, None),
            Err(error) => {
                if !self.ensure_still_launching_after_spawn_failure(task_id, attempt_no) {
                    return;
                }
                let _ = self.store.commit_spawn_not_started(
                    task_id,
                    attempt_no,
                    task.revision + 1,
                    &format!("受管实现输入准备失败：{error}"),
                    &now(),
                );
                return;
            }
        };
        let implementer_path: Option<PathBuf> = selection
            .as_ref()
            .map(|pair| pair.implementer.path.clone())
            .or_else(|| self.config.adapter_fixture.clone());
        let adapter_mode = implementer_path.is_some();
        let mut spawn_cmd = if let Some(command) = managed_cmd {
            command
        } else if let Some(fixture) = &implementer_path {
            let mut c = Command::new(fixture);
            c.arg("run");
            if selection.is_some() {
                // 按角色隔离的子进程环境（不改 daemon 自身环境）。
                c.envs(&self.config.implement_env);
                c.env("IMPLEMENT_ROUND", "1");
            }
            c
        } else {
            Command::new(&self.config.mock_worker)
        };
        if !managed {
            spawn_cmd.arg(&wt.path).args(&self.config.mock_worker_args);
        }
        spawn_cmd
            .stdout(if adapter_mode {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if managed {
            if !self.ensure_still_launching_after_spawn_failure(task_id, attempt_no) {
                return;
            }
            if stop.load(Ordering::SeqCst) {
                let _ = self.store.commit_spawn_not_started(
                    task_id,
                    attempt_no,
                    task.revision + 1,
                    "daemon 停止，受管实现从未启动",
                    &now(),
                );
                return;
            }
        }
        let child = match spawn_cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // 明确未启动（spawn 失败：worker 不存在或无法执行）：
                // 失败状态、事件、解封与审计同一事务提交——不留下
                // "任务 failed 却有 pending 标记"的半提交。
                eprintln!(
                    "relay daemon: attempt {task_id}#{attempt_no} 子进程启动失败：{e}（{}）",
                    self.config.mock_worker.display()
                );
                if review_active
                    && !self.ensure_still_launching_after_spawn_failure(task_id, attempt_no)
                {
                    return;
                }
                if let Err(commit_err) = self.store.commit_spawn_not_started(
                    task_id,
                    attempt_no,
                    task.revision + 1,
                    &format!("spawn 失败，worker 从未启动：{e}"),
                    &now(),
                ) {
                    // 整体回滚：任务停在 launching、标记保持 pending（失败
                    // 关闭，B 不被派发），需停机后经 gate confirm 处置。
                    eprintln!(
                        "relay daemon: attempt {task_id}#{attempt_no} 未启动提交失败（已整体回滚，保持阻断）：{commit_err}"
                    );
                }
                return;
            }
        };
        let mut child = child;
        // 适配器事件解析线程：stdout 行收敛为 started/termination/计数
        //（未知事件跳过并计数，不堆积；无效行计数）。
        let events: Arc<Mutex<AdapterRunEvents>> =
            Arc::new(Mutex::new(AdapterRunEvents::default()));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let max_frame = self.config.adapter_max_frame_bytes;
        let read_budget = Duration::from_millis(self.config.adapter_read_timeout_ms);
        let mut stdout_handle = if adapter_mode {
            let stdout = child.stdout.take().expect("adapter stdout piped");
            Some(if claude {
                crate::claude_review::spawn_reader(
                    stdout,
                    Arc::clone(&events),
                    done_tx.clone(),
                    read_budget,
                )
            } else {
                spawn_event_reader(
                    stdout,
                    Arc::clone(&events),
                    done_tx.clone(),
                    max_frame,
                    read_budget,
                )
            })
        } else {
            None
        };
        let pgid = child.id() as i32;
        let mut child = child;
        // 退出码在等待循环内赋值；panic/取消路径不读它。
        let exit_ok: bool;
        // guard 持有至 run_attempt 结束（取消可在 verifying 阶段回收整组）。
        let _owned_guard = OwnedGuard {
            owned: Arc::clone(&self.owned),
            key: (task_id.to_owned(), attempt_no),
        };
        self.owned
            .lock()
            .expect("owned 锁中毒")
            .insert((task_id.to_owned(), attempt_no), pgid);
        // 0) 同步点：已 spawn、尚未登记组（用于三窗口覆盖）。
        if self.stall("after_spawn_before_register", task_id, stop) {
            return;
        }
        // 1) 受管组登记（归属证据：pgid + 组长 pid + 启动时间）。写入
        //    在 spawn 之后——失败不撤销启动许可，但缺组记录时不得自动
        //    解封（缺进程记录）。此时 gate 标记仍 pending（仍阻断）。
        let group_registered = self
            .store
            .insert_managed_group(relay_store::ManagedGroupInput {
                task_id,
                attempt_no,
                pgid,
                leader_pid: pgid,
                leader_start: &Self::leader_start(pgid),
                now: &now(),
            })
            .is_ok();
        if !group_registered {
            // 组记录缺失：进程号身份无法核实——保守中断；回收未确认则
            // gate 保持 pending（不自动解封）。
            eprintln!(
                "relay daemon: attempt {task_id}#{attempt_no} 受管组登记失败（缺进程记录），转 interrupted"
            );
            let confirmed = self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
            self.reap_child(&mut child);
            if confirmed {
                // 可信退出证据：解封 gate（组已空）。
                let _ = self.store.release_dispatch_gate(
                    task_id,
                    attempt_no,
                    "组已确认退出（登记失败但退出已证实）",
                    &now(),
                );
            } else {
                let _ = self.store.set_gate_reason(
                    task_id,
                    attempt_no,
                    "受管组登记失败且退出未确认：缺进程记录，不自动解封",
                    &now(),
                );
            }
            let _ = self.store.transition(TransitionInput {
                task_id,
                expected_active_attempt: attempt_no,
                expected_revision: task.revision + 1,
                event: Event::MarkInterrupted,
                now: &now(),
            });
            return;
        }
        // 2) Spawned 状态推进与组确认同一事务：失败整体回滚（gate 标记
        //    仍 pending，继续阻断——不靠 spawn 后补写 blocking 兜底）。
        if self
            .store
            .confirm_spawn_running(task_id, attempt_no, task.revision + 1, &now())
            .is_err()
        {
            // 写回失败：有界回收；Confirmed（可信退出证据）→ 解封 +
            // SpawnFailed；Unconfirmed → gate 保持 pending + interrupted。
            let confirmed = self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
            self.reap_child(&mut child);
            let event = if confirmed {
                let _ = self.store.release_dispatch_gate(
                    task_id,
                    attempt_no,
                    "写回失败但组已确认退出",
                    &now(),
                );
                Event::SpawnFailed
            } else {
                let _ = self.store.set_gate_reason(
                    task_id,
                    attempt_no,
                    "spawn 写回失败且退出未确认，不自动解封",
                    &now(),
                );
                eprintln!(
                    "relay daemon: attempt {task_id}#{attempt_no} 写回失败且回收未证实退出，转 interrupted 阻塞"
                );
                Event::MarkInterrupted
            };
            let _ = self.store.transition(TransitionInput {
                task_id,
                expected_active_attempt: attempt_no,
                expected_revision: task.revision + 1,
                event,
                now: &now(),
            });
            if confirmed {
                self.confirm_workers_stopped(task_id, attempt_no);
            }
            return;
        }
        if self.stall("after_spawn", task_id, stop) {
            return;
        }
        // 5. 等待退出（可取消；总等待上限）。
        let mut child = child;
        let deadline =
            std::time::Instant::now() + Duration::from_millis(self.config.worker_timeout_ms);
        loop {
            if stop.load(Ordering::SeqCst) {
                // daemon 停止：有界回收受管进程组，不做状态推进（恢复
                // 分类处理；未证实退出由恢复归入 interrupted，重试阻断）。
                let confirmed = self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                    == ReclaimOutcome::Confirmed;
                self.reap_child(&mut child);
                if confirmed {
                    let _ = self
                        .store
                        .resolve_managed_group(task_id, attempt_no, &now());
                    let _ = self.store.release_dispatch_gate(
                        task_id,
                        attempt_no,
                        "停机回收已确认退出",
                        &now(),
                    );
                } else {
                    let _ = self.store.mark_group_blocking(
                        task_id,
                        attempt_no,
                        "daemon 停机回收未证实退出",
                        &now(),
                    );
                    let _ = self.store.set_gate_reason(
                        task_id,
                        attempt_no,
                        "停机回收未确认退出，不自动解封",
                        &now(),
                    );
                    eprintln!("relay daemon: 停机回收未证实退出（attempt {task_id}#{attempt_no}），恢复将阻断重试");
                }
                return;
            }
            // 取消竞态：cancelling → 同一有界回收逻辑（TERM→KILL）；
            // Unconfirmed 时仅回收子进程句柄，状态保持 cancelling 由
            // 取消发起方继续跟踪（不派新 attempt）。
            if let Ok(Some(t)) = self.store.get_task(task_id) {
                if t.state == State::Cancelling {
                    let confirmed =
                        self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                            == ReclaimOutcome::Confirmed;
                    self.reap_child(&mut child);
                    if confirmed {
                        self.confirm_workers_stopped(task_id, attempt_no);
                        let _ = self
                            .store
                            .resolve_managed_group(task_id, attempt_no, &now());
                        let _ = self.store.release_dispatch_gate(
                            task_id,
                            attempt_no,
                            "取消回收已确认退出",
                            &now(),
                        );
                    } else {
                        // 未证实退出：持久阻断 + 保持 cancelling（可追踪）。
                        let _ = self.store.mark_group_blocking(
                            task_id,
                            attempt_no,
                            "取消回收未证实退出",
                            &now(),
                        );
                        let _ = self.store.set_gate_reason(
                            task_id,
                            attempt_no,
                            "取消回收未确认退出，不自动解封",
                            &now(),
                        );
                        eprintln!(
                            "relay daemon: attempt {task_id}#{attempt_no} 取消回收未证实退出，保持 cancelling"
                        );
                    }
                    return;
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    // 组长退出 ≠ 整组退出：整组确认在循环外统一收口。
                    exit_ok = status.success();
                    break;
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        // 超时：有界回收后按 WorkerFailed 处理（收据/发布
                        // 不会发生；组回收未证实也照常分类 failed，但保留
                        // 日志线索）。
                        let confirmed =
                            self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                                == ReclaimOutcome::Confirmed;
                        if confirmed {
                            self.reap_child(&mut child);
                            let _ = self
                                .store
                                .resolve_managed_group(task_id, attempt_no, &now());
                            let _ = self.store.release_dispatch_gate(
                                task_id,
                                attempt_no,
                                "超时回收已确认退出",
                                &now(),
                            );
                            // 组已证实退出：按 WorkerFailed 分类（running
                            // → failed，可经 relay_retry 重试）。
                            self.fail_worker(
                                task_id,
                                attempt_no,
                                task.revision + 2,
                                "mock 子进程超时",
                            );
                        } else {
                            // 超时后仍有进程或探测不可确认：持久阻断 +
                            // MarkInterrupted（不进可重试失败态，不丢跟踪）。
                            let _ = self.store.mark_group_blocking(
                                task_id,
                                attempt_no,
                                "执行超时且回收未证实退出",
                                &now(),
                            );
                            let _ = self.store.set_gate_reason(
                                task_id,
                                attempt_no,
                                "执行超时且退出未确认，不自动解封",
                                &now(),
                            );
                            let _ = self.store.transition(TransitionInput {
                                task_id,
                                expected_active_attempt: attempt_no,
                                expected_revision: task.revision + 2,
                                event: Event::MarkInterrupted,
                                now: &now(),
                            });
                            eprintln!(
                                "relay daemon: attempt {task_id}#{attempt_no} 超时回收未证实退出，转 interrupted 阻塞"
                            );
                        }
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(_) => {
                    self.fail_verifying(
                        task_id,
                        attempt_no,
                        task.revision + 2,
                        "mock 子进程等待失败",
                    );
                    return;
                }
            }
        }
        // 自动链路激活时 gate 继续持有到轮次结束；其后任一失败出口都要
        // 在确认受控进程已退出后解封（避免"任务失败却留下 pending"）。
        let release_held_gate = |reason: &str| {
            let _ = self
                .store
                .release_dispatch_gate(task_id, attempt_no, reason, &now());
            self.confirm_workers_stopped(task_id, attempt_no);
        };
        // 统一收口：组长退出后必须确认整组退出才可继续。未确认 →
        // 持久阻断 + 任务失败（不产候选）。
        if !self.settle_after_leader_exit(
            task_id,
            attempt_no,
            task.revision + 2,
            pgid,
            "组长退出但整组退出未确认：持久阻断",
            false,
        ) {
            if let Some(handle) = stdout_handle.take() {
                // 不无期限 join：读者线程有自身上限，收割有界。
                let _ = handle.is_finished();
            }
            return;
        }
        drop(_owned_guard);
        if !exit_ok && !adapter_mode {
            // Without adapter output there is no terminal frame to classify.
            release_held_gate("实现者非零退出：受控进程已确认退出");
            let _ = self.store.transition(TransitionInput {
                task_id,
                expected_active_attempt: attempt_no,
                expected_revision: task.revision + 2,
                event: Event::WorkerFailed,
                now: &now(),
            });
            if let Some(handle) = stdout_handle.take() {
                let _ = handle.is_finished();
            }
            return;
        }
        // 5.5 适配器事件分类（05）。普通 mock 路径（无 stdout）跳过。
        if adapter_mode {
            // 有界等待事件读完：后代进程持续持有 stdout 管道时按上限
            // 结束（不等管道关闭）。
            match done_rx.recv_timeout(read_budget) {
                Ok(()) => {
                    if let Some(handle) = stdout_handle.take() {
                        let _ = handle.join();
                    }
                }
                Err(_) => {
                    // 等待上限：读取未完成（管道可能被后代持有）。整组退
                    // 出确认交给统一收口；未确认则持久阻断。
                    {
                        let mut guard = events.lock().expect("adapter events 锁中毒");
                        guard.read_timeouts += 1;
                    }
                    if !self.settle_after_leader_exit(
                        task_id,
                        attempt_no,
                        task.revision + 2,
                        pgid,
                        "适配器 stdout 未在上限内读完，整组退出未确认：持久阻断",
                        false,
                    ) {
                        return;
                    }
                    release_held_gate("适配器 stdout 未在上限内读完：受控进程已确认退出");
                    self.fail_worker(
                        task_id,
                        attempt_no,
                        task.revision + 2,
                        "适配器 stdout 未在上限内读完（管道可能被后代持有）",
                    );
                    return;
                }
            }
            let classification = {
                let guard = events.lock().expect("adapter events 锁中毒");
                classify_events(&guard, exit_ok)
            };
            if let Err(reason) = classification {
                self.fail_adapter(task_id, attempt_no, task.revision + 2, &reason);
                return;
            }
        } else if let Some(handle) = stdout_handle.take() {
            let _ = handle.join();
        }
        if let Some(edits) = edit_request {
            if stop.load(Ordering::SeqCst) {
                release_held_gate("daemon 停止，未应用受管编辑");
                return;
            }
            if !self.ensure_still_running(task_id, attempt_no) {
                return;
            }
            let (count, value, evidence) = {
                let mut events = events.lock().expect("adapter events lock");
                (
                    events.review_verdict_count,
                    events.review_verdict.take(),
                    events.provider_evidence.take(),
                )
            };
            if count != 1 {
                self.fail_adapter(
                    task_id,
                    attempt_no,
                    task.revision + 2,
                    "实现结论帧必须恰好一个",
                );
                return;
            }
            if self.config.live_routes.is_some() {
                if let Err(code) = crate::live_routes::record_usage(
                    &self.store,
                    task_id,
                    attempt_no,
                    crate::live_routes::Stage::Implement,
                    evidence.as_ref(),
                    None,
                    None,
                ) {
                    self.fail_adapter(task_id, attempt_no, task.revision + 2, code);
                    return;
                }
            }
            let outcome = value
                .as_ref()
                .ok_or_else(|| std::io::Error::other("EDIT_SCHEMA"))
                .and_then(|v| edits.apply(v));
            if let Err(error) = outcome {
                self.workflow_fail(
                    task_id,
                    attempt_no,
                    task.revision + 2,
                    &format!("受管实现拒绝：{error}"),
                );
                return;
            }
        }
        // 6. 验证 + 自动链路（15）：
        //    - 有独立评审者：实现 → 本地验证 → 评审（被退回则返工一轮 →
        //      再验证 → 再评审，到上限即停）。轮次在 running 态内完成，
        //      最终结果才推进状态机；全部受控子进程确认退出后解封 gate。
        //    - 无评审者（单实现者 / implementation_only）：进入 verifying，
        //      真实差异采集 + 真实验证流程收据。
        let mut workflow_summary: Option<Value> = None;
        let (collected, receipt, started_at);
        if review_active {
            let pair = selection.as_ref().expect("review_active implies selection");
            if let Err(e) = self.store.finish_stage(
                first_stage,
                task.revision + 2,
                relay_store::StageFinish::Implemented,
                &now(),
            ) {
                self.workflow_fail(
                    task_id,
                    attempt_no,
                    task.revision + 2,
                    &format!("首轮实现阶段无法提交：{e}"),
                );
                return;
            }
            let output = match self.run_review_workflow(
                task_id,
                attempt_no,
                &attempt_id,
                &wt,
                &plan,
                &scope,
                &verify_config,
                pair,
                stop,
            ) {
                Some(output) => output,
                None => return,
            };
            release_held_gate("自动链路轮次结束：受控子进程均已确认退出");
            if self
                .store
                .transition(TransitionInput {
                    task_id,
                    expected_active_attempt: attempt_no,
                    expected_revision: task.revision + 2,
                    event: Event::WorkerSucceeded,
                    now: &now(),
                })
                .is_err()
            {
                return;
            }
            collected = output.collected;
            receipt = output.receipt;
            started_at = output.started_at;
            workflow_summary = Some(output.summary);
        } else {
            if self
                .store
                .transition(TransitionInput {
                    task_id,
                    expected_active_attempt: attempt_no,
                    expected_revision: task.revision + 2,
                    event: Event::WorkerSucceeded,
                    now: &now(),
                })
                .is_err()
            {
                release_held_gate("实现已退出，验证未启动");
                return;
            }
            match self.verify_once(
                task_id,
                attempt_no,
                task.revision + 3,
                &attempt_id,
                &wt.path,
                &plan,
                &scope,
                &verify_config,
                stop,
            ) {
                Some((c, r, started)) => {
                    collected = c;
                    receipt = r;
                    started_at = started;
                }
                None => return,
            }
        }
        // 7. handoff（06：summary ≤ 2 KiB、引用由 Relay 生成、16 KiB 上限）。
        //    15：交付包附评审结论、返工次数与实际路由。
        let mut handoff = json!({
            "schema_version": 1,
            "attempt": attempt_id,
            "base_oid": plan.base_oid,
            "summary": plan.brief,
            "files": collected.changes.iter()
                .map(|c| json!({"path": c.path, "action": match c.action {
                    relay_git::ChangeAction::Added => "added",
                    relay_git::ChangeAction::Modified => "modified",
                    relay_git::ChangeAction::Deleted => "deleted",
                }}))
                .collect::<Vec<_>>(),
            "test_receipt_sha256": sha256_hex(receipt.json.as_bytes()),
            "diff_sha256": sha256_hex(collected.patch.as_bytes()),
            "open_risks": [],
            "compliance": "approved",
        });
        if let Some(summary) = &workflow_summary {
            handoff["workflow"] = summary.clone();
        }
        let handoff_json = serde_json::to_string(&handoff).unwrap_or_default();
        // 7. 持久发布（原子不替换）+ 同事务登记；登记失败回滚目录。
        let artifact_root = self.config.managed_root.join("artifacts");
        if std::fs::create_dir_all(&artifact_root).is_err() {
            self.fail_verifying(task_id, attempt_no, task.revision + 3, "制品根创建失败");
            return;
        }
        let input = relay_git::ArtifactInput {
            attempt: &attempt_id,
            base_oid: &plan.base_oid,
            created_at: &started_at,
            collected: &collected,
        };
        let paths =
            match publish_artifact(&artifact_root, &attempt_id, input, &receipt, &handoff_json) {
                Ok(p) => p,
                Err(e) => {
                    self.fail_verifying(
                        task_id,
                        attempt_no,
                        task.revision + 3,
                        &format!("制品发布失败：{e}"),
                    );
                    return;
                }
            };
        let manifest_bytes = std::fs::read(&paths.manifest_path);
        let patch_bytes = std::fs::read(&paths.patch_path);
        let receipt_bytes = std::fs::read(&paths.receipt_path);
        let handoff_bytes = std::fs::read(paths.dir.join("handoff.json"));
        let (m_b, p_b, r_b, h_b) = match (manifest_bytes, patch_bytes, receipt_bytes, handoff_bytes)
        {
            (Ok(m), Ok(p), Ok(r), Ok(h)) => (m, p, r, h),
            (err, _, _, _) => {
                let _ = std::fs::remove_dir_all(&paths.dir);
                self.fail_verifying(
                    task_id,
                    attempt_no,
                    task.revision + 3,
                    &format!("制品回读失败：{err:?}"),
                );
                return;
            }
        };
        if self.stall("published_before_register", task_id, stop) {
            return;
        }
        let register = self
            .store
            .register_artifact_and_transition(ArtifactRegisterInput {
                task_id,
                attempt_no,
                expected_revision: task.revision + 3,
                artifact_dir: paths.dir.to_string_lossy().as_ref(),
                manifest_sha256: &sha256_hex(&m_b),
                patch_sha256: &sha256_hex(&p_b),
                receipt_sha256: &sha256_hex(&r_b),
                handoff_sha256: &sha256_hex(&h_b),
                now: &now(),
            });
        if let Err(e) = register {
            // 同事务失败：移除已落盘候选（未登记不可见，不阻塞重试）。
            let _ = std::fs::remove_dir_all(&paths.dir);
            self.fail_verifying(
                task_id,
                attempt_no,
                task.revision + 3,
                &format!("制品登记失败：{e}"),
            );
        }
    }

    /// 单实现者路径的验证（verifying 态）：真实差异采集 + 真实验证流程
    /// 收据。任一步失败按 VerificationFailed 收口并返回 None。
    #[allow(clippy::too_many_arguments)]
    fn verify_once(
        &self,
        task_id: &str,
        attempt_no: u32,
        expected_revision: u64,
        attempt_id: &str,
        worktree: &Path,
        plan: &relay_store::PlanRecord,
        scope: &WriteScope,
        verify_config: &VerifyConfig,
        stop: &AtomicBool,
    ) -> Option<(CollectedDiff, TestReceipt, String)> {
        use crate::verify_process::VerifyProcess;
        use relay_git::{receipt::run_and_build_receipt_with, verify::run_rule_supervised};
        use relay_git::{ReceiptError, VerifyError};
        let supervisor = VerifyProcess {
            service: self,
            task_id,
            attempt_no,
            stop,
        };
        let collected = match collect_diff(worktree, &plan.base_oid, scope) {
            Ok(c) => c,
            Err(e) => {
                supervisor.release_after_stop();
                self.fail_verifying(
                    task_id,
                    attempt_no,
                    expected_revision,
                    &format!("差异采集失败：{e}"),
                );
                return None;
            }
        };
        let started_at = now_rfc3339();
        let receipt = match run_and_build_receipt_with(
            verify_config,
            &plan.rule_ids,
            worktree,
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
                supervisor.release_after_stop();
                self.fail_verifying(
                    task_id,
                    attempt_no,
                    expected_revision,
                    &format!("收据生成失败：{e}"),
                );
                return None;
            }
        };
        supervisor.release_after_stop();
        if receipt.status != "pass" {
            // 验证失败：不出具可验收制品（06）。
            self.fail_verifying(
                task_id,
                attempt_no,
                expected_revision,
                "验证失败（收据非通过）",
            );
            return None;
        }
        Some((collected, receipt, started_at))
    }

    /// preparing 阶段失败（worktree/scope/规则配置）。
    fn fail_preparing(&self, task_id: &str, attempt_no: u32, expected_revision: u64, reason: &str) {
        let _ = self.store.transition(TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision,
            event: Event::PreparationFailed,
            now: &now_rfc3339(),
        });
        eprintln!("relay daemon: attempt {task_id}#{attempt_no} 准备失败：{reason}");
    }

    /// Called after the process group is confirmed absent. Store only the
    /// classifier's bounded messages and stable codes, never provider output.
    pub(crate) fn fail_adapter(&self, task_id: &str, attempt_no: u32, revision: u64, reason: &str) {
        if let Err(error) = self.store.fail_worker_with_reason(
            TransitionInput {
                task_id,
                expected_active_attempt: attempt_no,
                expected_revision: revision,
                event: Event::WorkerFailed,
                now: &now_rfc3339(),
            },
            reason,
        ) {
            eprintln!("relay daemon: failed to persist adapter diagnostic: {error}");
        }
        let _ = self.store.release_dispatch_gate(
            task_id,
            attempt_no,
            "适配器执行失败：受控进程已确认退出",
            &now_rfc3339(),
        );
        self.confirm_workers_stopped(task_id, attempt_no);
    }

    /// running 阶段失败（适配器事件/终态不合规）：WorkerFailed
    /// （running → failed），不发布、不验收。
    pub(crate) fn fail_worker(
        &self,
        task_id: &str,
        attempt_no: u32,
        expected_revision: u64,
        reason: &str,
    ) {
        let _ = self.store.transition(TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision,
            event: Event::WorkerFailed,
            now: &now_rfc3339(),
        });
        eprintln!("relay daemon: attempt {task_id}#{attempt_no} 执行失败：{reason}");
    }

    /// verifying 阶段失败（差异/收据/发布/登记）。
    fn fail_verifying(&self, task_id: &str, attempt_no: u32, expected_revision: u64, reason: &str) {
        let _ = self.store.transition(TransitionInput {
            task_id,
            expected_active_attempt: attempt_no,
            expected_revision,
            event: Event::VerificationFailed,
            now: &now_rfc3339(),
        });
        eprintln!("relay daemon: attempt {task_id}#{attempt_no} 验证失败：{reason}");
    }

    /// 回收确认归属的进程组：TERM → 等待退出确认（上限 5s）。
    /// 未证实退出时保留 cancelling 状态、不推进、不派下一 attempt。
    fn stop_owned_worker(&self, task_id: &str, attempt_no: u32) {
        // 该 attempt 当前受派发门跟踪的全部进程组：内存登记的实现者组
        //（pgid 的移除由 OwnedGuard 在 attempt 结束时统一处理）+ 待确认
        // 标记上的子进程组（探测/评审/返工）。两组都确认退出才兑现
        // cancelled；任一未确认即保持 cancelling 并持久阻断。
        let owned = self
            .owned
            .lock()
            .expect("owned 锁中毒")
            .get(&(task_id.to_owned(), attempt_no))
            .copied();
        let gates = match self.store.list_pending_gates() {
            Ok(gates) => gates,
            Err(_) => return,
        };
        let gate = gates
            .iter()
            .find(|g| g.task_id == task_id && g.attempt_no == attempt_no);
        // In the verification/review path the executor owns both the spawn
        // window and the child handle; let it settle cancellation. In launching,
        // an unregistered spawn or probe is likewise not evidence of absence.
        if owned.is_none() && gate.is_some() {
            return;
        }
        let gate_pgid = gate.map_or(0, |gate| gate.pgid);
        let now = now_rfc3339();
        let mut all_confirmed = true;
        // 统一有界回收：TERM → 期限；仍存活 → SIGKILL → 期限。
        if let Some(pgid) = owned {
            if self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed
            {
                let _ = self.store.resolve_managed_group(task_id, attempt_no, &now);
            } else {
                all_confirmed = false;
                let _ = self.store.mark_group_blocking(
                    task_id,
                    attempt_no,
                    "取消回收期限到仍未证实退出",
                    &now,
                );
                eprintln!(
                    "relay daemon: attempt {task_id}#{attempt_no} 的进程组 {pgid} 期限到仍未证实退出"
                );
            }
        }
        if gate_pgid > 0 {
            if self.reclaim_group(gate_pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed
            {
                let _ = self.store.set_gate_pgid(task_id, attempt_no, 0, &now);
            } else {
                all_confirmed = false;
                eprintln!(
                    "relay daemon: attempt {task_id}#{attempt_no} 的子进程组 {gate_pgid} 期限到仍未证实退出"
                );
            }
        }
        if all_confirmed {
            // Confirmed 才推进 cancelled。
            // An active stage may be between permit and spawn, or running local
            // verification. Only its executor can attest that this window closed.
            if matches!(self.store.has_active_stage(task_id, attempt_no), Ok(false)) {
                self.confirm_workers_stopped(task_id, attempt_no);
            }
        } else {
            // Unconfirmed 保留 cancelling——不报告已取消、不派下一 attempt、
            // 不丢失跟踪（进程号留在受管组/待确认标记上）。
            let _ = self.store.set_gate_reason(
                task_id,
                attempt_no,
                "取消回收期限到仍未证实退出，不自动解封",
                &now,
            );
            eprintln!(
                "relay daemon: attempt {task_id}#{attempt_no} 取消回收未全部证实退出，保持 cancelling 并持久阻断"
            );
        }
    }

    fn ensure_still_launching_after_spawn_failure(&self, task_id: &str, attempt_no: u32) -> bool {
        if matches!(self.store.get_task(task_id), Ok(Some(t)) if
            t.active_attempt == attempt_no && t.state == State::Cancelling)
        {
            self.confirm_workers_stopped(task_id, attempt_no);
            let _ = self.store.release_dispatch_gate(
                task_id,
                attempt_no,
                "启动失败，已确认未启动",
                &now_rfc3339(),
            );
            return false;
        }
        true
    }

    pub(crate) fn confirm_workers_stopped(&self, task_id: &str, attempt_no: u32) {
        if let Ok(Some(t)) = self.store.get_task(task_id) {
            if t.state == State::Cancelling {
                let _ = self.store.transition(TransitionInput {
                    task_id,
                    expected_active_attempt: attempt_no,
                    expected_revision: t.revision,
                    event: Event::AllOwnedWorkersStopped,
                    now: &now_rfc3339(),
                });
            }
        }
    }
    /// 组长进程启动时间（归属证据；ps 不可用记 unknown）。
    fn leader_start(pid: i32) -> String {
        std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_else(|_| "unknown".to_owned())
    }

    /// 适配器 probe：以 probe 模式运行夹具，解析身份与能力声明。
    /// 适配器 probe（05/16）：以 `probe` 模式运行目录项，解析身份、角色与
    /// 能力声明。`gate` 给出 (task_id, attempt_no) 时（派发路径）把 probe
    /// 的进程组记到该 attempt 的待确认标记上；None（预览路径）不登记。
    /// 有界：等待上限、读取上限、字节上限；超时即有界回收，整组退出
    /// 是否确认决定 `group_unconfirmed`。
    pub(crate) fn adapter_probe(
        &self,
        fixture: &Path,
        gate: Option<(&str, u32)>,
    ) -> Result<AdapterProbeReport, ProbeError> {
        let mut child = Command::new(fixture)
            .arg("probe")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| ProbeError::exited(format!("无法启动 probe：{e}")))?;
        let pgid = child.id() as i32;
        let deadline =
            std::time::Instant::now() + Duration::from_millis(self.config.adapter_probe_timeout_ms);
        let reclaim_and_settle = |child: &mut Child, reason: &str| -> Option<ProbeError> {
            // 整组退出确认（统一监管：组长退出 ≠ 整组退出）；未确认 → 有界
            // 回收后再探测，仍不确认则如实报告（派发路径 gate 保持 pending）。
            let confirmed = self.confirm_group_exit(
                pgid,
                Duration::from_millis(self.config.group_exit_budget_ms),
            ) || self.reclaim_group(pgid, self.term_budget(), self.kill_budget())
                == ReclaimOutcome::Confirmed;
            self.reap_child(child);
            if !confirmed {
                return Some(ProbeError::unconfirmed(reason, pgid));
            }
            if let Some((task_id, attempt_no)) = gate {
                let _ = self
                    .store
                    .set_gate_pgid(task_id, attempt_no, 0, &now_rfc3339());
            }
            None
        };
        // 派发路径：持久归属证据（重启后据此探测确认退出）。登记失败即
        // 失去可核实的进程号身份——保守回收。
        if let Some((task_id, attempt_no)) = gate {
            if let Err(e) = self
                .store
                .set_gate_pgid(task_id, attempt_no, pgid, &now_rfc3339())
            {
                let err = reclaim_and_settle(&mut child, &format!("probe 进程组登记失败：{e}"))
                    .unwrap_or_else(|| ProbeError::exited(format!("probe 进程组登记失败：{e}")));
                return Err(err);
            }
        }
        // 有界读取（与 run 共用）：超限即停，等待上限即超时。
        let reader = child.stdout.take().expect("probe stdout piped");
        let read = read_bounded(reader, self.config.adapter_max_frame_bytes, deadline);
        // 收割：子进程退出或有界回收（不无期限 join）。
        let mut exited = None;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited = Some(status);
                    break;
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    let err = reclaim_and_settle(&mut child, &format!("probe 等待失败：{e}"))
                        .unwrap_or_else(|| ProbeError::exited(format!("probe 等待失败：{e}")));
                    return Err(err);
                }
            }
        }
        let timed_out = exited.is_none();
        if let Some(err) = reclaim_and_settle(&mut child, "probe 整组退出未确认（成员残留）")
        {
            return Err(err);
        }
        if timed_out {
            return Err(ProbeError::exited(format!(
                "probe 等待超过 {} ms 上限",
                self.config.adapter_probe_timeout_ms
            )));
        }
        let status = exited.expect("probe 退出状态");
        if !status.success() {
            return Err(ProbeError::exited(format!(
                "probe 退出 {:?}",
                status.code()
            )));
        }
        if read.oversized {
            return Err(ProbeError::exited(format!(
                "probe 输出超过 {} 字节上限",
                self.config.adapter_max_frame_bytes
            )));
        }
        if read.timed_out {
            return Err(ProbeError::exited("probe 读取等待超时".to_owned()));
        }
        if read.read_error {
            return Err(ProbeError::exited("probe 输出读取失败".to_owned()));
        }
        let text = String::from_utf8_lossy(&read.bytes);
        let value: Value = serde_json::from_str(text.trim())
            .map_err(|e| ProbeError::exited(format!("probe 输出不是 JSON：{e}")))?;
        if value["type"] != json!("probe") {
            return Err(ProbeError::exited("probe 输出类型不符".to_owned()));
        }
        let list = |key: &str| -> Vec<String> {
            value[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(AdapterProbeReport {
            binary: value["binary"].as_str().unwrap_or_default().to_owned(),
            version: value["version"].as_str().unwrap_or_default().to_owned(),
            // 空或空白的身份不算"可核实"：按未核实处理（16：身份未知不能
            // 自动推荐/自动验收）。
            actual_model: value["actual_model"]
                .as_str()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned),
            capabilities: list("capabilities"),
            roles: list("roles"),
        })
    }

    pub(crate) fn term_budget(&self) -> Duration {
        Duration::from_millis(self.config.reclaim_term_ms)
    }

    pub(crate) fn kill_budget(&self) -> Duration {
        Duration::from_millis(self.config.reclaim_kill_ms)
    }

    /// 有界收割直接子进程：组已证实退出时 wait 立即返回；预算内未收割
    /// （替身谎报/异常情形）则放弃收割句柄——绝不无期限阻塞。
    pub(crate) fn reap_child(&self, child: &mut Child) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        // 丢弃句柄不等待：受管进程状态由追踪记录保留。
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return false,
            }
        }
    }

    /// Ok(false)（组已消失）才算退出；探测失败按"仍存活"对待直到预算
    /// 用尽——探测错误不产生已退出证据。
    fn wait_group_gone(&self, pgid: i32, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            match self.group_control.probe(pgid) {
                Ok(false) => return true,
                Ok(true) | Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 有界进程组回收（03：只有确认整个组退出才可标 cancelled）。
    /// TERM → `term_budget`；仍存活 → SIGKILL → `kill_budget`。任一阶段
    /// 探测到组消失即 Confirmed；预算用尽仍未证实退出 → Unconfirmed
    /// （未知结果保留阻塞，不丢失跟踪、不派新 attempt）。
    pub(crate) fn reclaim_group(
        &self,
        pgid: i32,
        term_budget: Duration,
        kill_budget: Duration,
    ) -> ReclaimOutcome {
        // 信号失败不是证据：term/kill 的 Err 不产生任何"已退出"结论。
        let _ = self.group_control.term(pgid);
        if self.wait_group_gone(pgid, term_budget) {
            return ReclaimOutcome::Confirmed;
        }
        let _ = self.group_control.kill(pgid);
        if self.wait_group_gone(pgid, kill_budget) {
            return ReclaimOutcome::Confirmed;
        }
        ReclaimOutcome::Unconfirmed
    }
}

fn store_error_to_biz(err: relay_store::StoreError) -> BizError {
    use relay_store::StoreError;
    match &err {
        StoreError::NotFound { task_id } => BizError::not_found(format!("任务 {task_id} 不存在")),
        StoreError::PlanExpired { plan_id } => {
            BizError::new("PLAN_EXPIRED", format!("计划 {plan_id} 已过期"), "new_plan")
        }
        StoreError::PlanUsed { .. } => BizError::new("PLAN_USED", err.to_string(), "reuse_task"),
        StoreError::PlanStale { .. } => BizError::new("PLAN_STALE", err.to_string(), "new_plan"),
        StoreError::IdempotencyConflict { .. } => {
            BizError::new("IDEMPOTENCY_CONFLICT", err.to_string(), "keep_request_id")
        }
        StoreError::StaleRevision { .. } => {
            BizError::new("STALE_REVISION", err.to_string(), "refresh_status")
        }
        StoreError::RetryNotAllowed { .. } => {
            BizError::new("WRONG_STATE", err.to_string(), "confirm_exit_first")
        }
        StoreError::WrongState { .. } => BizError::wrong_state(err.to_string()),
        StoreError::InvalidInput(_) => BizError::invalid(err.to_string()),
        _ => BizError::internal(err.to_string()),
    }
}

/// git rev-parse HEAD（只读主 checkout；M1 不引入额外 Git 封装）。
/// 进程控制边界（可注入）：组信号与组存活探测。生产用
/// [`RealGroupControl`]（relay-sys）；测试替身可模拟信号失败、探测
/// 错误与"子进程始终不退出"。
pub trait GroupControl: Send + Sync {
    fn term(&self, pgid: i32) -> std::io::Result<()>;
    fn kill(&self, pgid: i32) -> std::io::Result<()>;
    /// Ok(true) = 组内仍有存活成员；Ok(false) = 组已消失（ESRCH）；
    /// Err = 探测失败（权限/IO，不可确认）。
    fn probe(&self, pgid: i32) -> std::io::Result<bool>;
}

/// 生产实现：relay-sys 原语。
pub struct RealGroupControl;

impl GroupControl for RealGroupControl {
    fn term(&self, pgid: i32) -> std::io::Result<()> {
        relay_sys::killpg_term(pgid)
    }
    fn kill(&self, pgid: i32) -> std::io::Result<()> {
        relay_sys::killpg_kill(pgid)
    }
    fn probe(&self, pgid: i32) -> std::io::Result<bool> {
        relay_sys::group_has_members(pgid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReclaimOutcome {
    /// 整组已证实退出。
    Confirmed,
    /// 期限到仍未证实退出：保留 cancelling/阻塞，不报告已取消。
    Unconfirmed,
}

/// Ok(false)（组已消失）才算退出；探测失败按"仍存活"对待直到预算
/// 用尽——探测错误不产生已退出证据。
/// 当前时间 UTC RFC3339（秒精度，Z 时区）。
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

fn plus_seconds(rfc3339: &str, secs: u64) -> Result<String, String> {
    let bytes = rfc3339.as_bytes();
    let fmt_err = || "expires_at 不是有效的 UTC RFC3339（YYYY-MM-DDTHH:MM:SSZ）".to_owned();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(fmt_err());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64, String> {
        std::str::from_utf8(&bytes[r])
            .map_err(|_| fmt_err())?
            .parse::<i64>()
            .map_err(|_| fmt_err())
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, m, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 59 {
        return Err(fmt_err());
    }
    // days_from_civil（Howard Hinnant）。
    let y: i64 = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let epoch = (days * 86_400) + (h * 3600 + m * 60 + s);
    Ok(format_rfc3339((epoch + secs as i64) as u64))
}

/// epoch 秒 → RFC3339（Z）。civil-from-days 算法（Howard Hinnant）。
fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mth <= 2 { y + 1 } else { y };
    format!("{year:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// 短随机 opaque 后缀（时间 + 计数器 + pid 的十六进制；仅用于 ID）。
fn short_id() -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        nanos,
        COUNTER.fetch_add(1, Ordering::SeqCst),
        std::process::id()
    )
}

#[cfg(test)]
mod quota_tests {
    use super::managed_size;
    use std::fs;

    #[test]
    fn managed_size_counts_files_and_stops_at_limit() {
        let root = std::env::temp_dir().join(format!(
            "relay-quota-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("artifacts/a")).unwrap();
        fs::write(root.join("artifacts/a/file"), b"12345").unwrap();
        fs::write(root.join("top"), b"123").unwrap();
        assert_eq!(managed_size(&root, 1024), 8);
        assert_eq!(managed_size(&root, 6), 8, "limit is checked after a file");
        fs::remove_dir_all(root).ok();
    }
}
