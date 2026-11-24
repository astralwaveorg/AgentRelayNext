//! M2 离线适配器样板（05）：以固定模拟输入验证适配器边界的判定规则，
//! 不启用真实模型、不读凭据、不发网络请求。用例覆盖模型身份核实、
//! 权限声明合规、认证失败/限流停止（不切换账号或计费路线）、诊断不泄
//! 露凭据、低磁盘不领取。
//!
//! 边界说明：本样板证明判定逻辑本身；不能证明真实 Claude Code 的权限
//! 限制、真实模型身份或真实套餐资格（05 §M2：以非计费协议夹具核对后
//! 才跑真实用例）。Claude Code + MiniMax 保持候选且默认禁用。

use serde_json::{json, Value};

/// 候选路线注册表（05 §M2）：默认禁用；启用需官方依据与现场验证。
pub const CANDIDATE_ROUTES: &[(&str, bool, &[&str])] = &[
    (
        "claude_code_minimax",
        false,
        &[
            "Anthropic 分发条件对第三方推理凭据（MiniMax）适用性的书面依据",
            "用户批准的 MiniMax 套餐凭据组合（existing_subscription，用户在原生产品内核对可用性）",
            "官方 Claude Code 版本进入验证矩阵；非交互参数组合经非计费夹具核对",
            "08 T13–T15 现场验证（权限 canary 负例、auth fail 不回退、隐私标记不入日志）",
        ],
    ),
    (
        "opencode_minimax",
        false,
        &[
            "OpenCode 2.3.0 可连接本地合成端点，但实际响应模型未进入 CLI 事件",
            "默认拒绝仍有请求声明 skill；需核对附加请求、工具权限和请求预算",
            "真实 Token Plan、执行隔离与 08 T13–T15 尚未放行",
        ],
    ),
];

/// 模拟适配器帧（05 事件收敛面的最小子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimFrame {
    /// 适配器从可靠响应读出的实际模型标识。
    Identity { actual_model: String },
    /// 认证失败。
    AuthFailed { provider: String },
    /// 限流/配额停止。
    RateLimited { reason: String },
    /// 诊断文本（可能包含敏感标记）。
    Diagnostic { text: String },
    /// 权限声明（适配器启动时的能力清单）。
    PermissionDeclaration { capabilities: Vec<String> },
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 通过（合规/身份一致/无泄漏）。
    Pass,
    /// 不合规：拒绝，不进入待验收。
    Reject,
    /// 停止：认证失败或限流——不切换账号、不改计费路线。
    Stop,
    /// 警示（如身份"未核实"）：不当匹配对待。
    Warn,
}

/// 清洗诊断文本：过滤明显凭据标记（假 canary 用于验证清洗链路）。
pub fn redact_diagnostic(text: &str) -> String {
    let mut out = text.to_owned();
    for marker in ["sk-canary-", "AKIA", "password="] {
        if let Some(idx) = out.find(marker) {
            let end = out[idx..]
                .find(|c: char| c.is_whitespace())
                .map(|i| idx + i)
                .unwrap_or(out.len());
            out.replace_range(idx..end, "[已过滤]");
        }
    }
    out
}

/// 配置的模型身份与适配器上报的实际身份核对（05：configured ≠ actual
/// 为不合规；无法核实显示"未核实"，不当匹配对待）。
pub fn check_model_identity(configured: &str, actual: Option<&str>) -> (Verdict, String) {
    match actual {
        Some(a) if a == configured => (Verdict::Pass, format!("实际身份与配置一致：{a}")),
        Some(a) => (
            Verdict::Reject,
            format!("模型身份不匹配：配置 {configured}，实际 {a}（不合规，不进入待验收）"),
        ),
        None => (
            Verdict::Warn,
            "实际身份未核实；不得当作与配置匹配".to_owned(),
        ),
    }
}

/// 权限声明合规检查（05：仅文件读取/编辑；禁 shell、联网、外部 MCP、
/// 子代理）。声明含禁用能力即拒绝。
pub fn check_permission_declaration(capabilities: &[String]) -> (Verdict, String) {
    const FORBIDDEN: [&str; 4] = ["shell", "network", "external_mcp", "subagent"];
    let violations: Vec<&String> = capabilities
        .iter()
        .filter(|c| FORBIDDEN.iter().any(|f| c.to_lowercase().contains(f)))
        .collect();
    if violations.is_empty() {
        (Verdict::Pass, "权限声明合规（仅文件读取/编辑）".to_owned())
    } else {
        (
            Verdict::Reject,
            format!("权限声明不合规：{violations:?}（拒绝启动）"),
        )
    }
}

/// 认证失败 / 限流：一律停止，不切换账号或计费路线（05：不自动转备用；
/// 禁止自动转独立额度）。
pub fn check_stop_conditions(frame: &SimFrame) -> (Verdict, String) {
    match frame {
        SimFrame::AuthFailed { provider } => (
            Verdict::Stop,
            format!("认证失败（{provider}）：停止，不切换账号或计费路线"),
        ),
        SimFrame::RateLimited { reason } => (
            Verdict::Stop,
            format!("限流停止：{reason}（不自动转备用路线）"),
        ),
        _ => (
            Verdict::Reject,
            "停止条件检查只接受 AuthFailed/RateLimited".to_owned(),
        ),
    }
}

/// 低磁盘领取门：free < min → 不领取（任务保持 queued，不误报失败）。
pub fn check_disk_gate(free_bytes: u64, min_free_bytes: u64) -> (Verdict, String) {
    if min_free_bytes == 0 {
        return (Verdict::Pass, "磁盘门未启用".to_owned());
    }
    if free_bytes >= min_free_bytes {
        (
            Verdict::Pass,
            format!("磁盘可用 {free_bytes} 字节 ≥ 下限，允许领取"),
        )
    } else {
        (
            Verdict::Stop,
            format!(
                "磁盘可用 {free_bytes} 字节低于下限 {min_free_bytes}：不领取下一任务（保持 queued）"
            ),
        )
    }
}

/// 全部样板用例（固定模拟输入；一条命令跑完）。
pub fn run_sample_cases() -> Vec<(&'static str, Verdict, String)> {
    let mut cases = Vec::new();
    let push = |cases: &mut Vec<(&'static str, Verdict, String)>,
                name: &'static str,
                got: (Verdict, String),
                want: Verdict| {
        let pass = got.0 == want;
        cases.push((
            name,
            got.0,
            format!("{}：{}", if pass { "PASS" } else { "FAIL" }, got.1),
        ));
    };

    // 1. 模型身份可核实（一致）。
    push(
        &mut cases,
        "identity_match",
        check_model_identity("fake-adapter-v1", Some("fake-adapter-v1")),
        Verdict::Pass,
    );
    // 2. 模型身份不匹配 → 拒绝。
    push(
        &mut cases,
        "identity_mismatch",
        check_model_identity("fake-adapter-v1", Some("some-other-model")),
        Verdict::Reject,
    );
    // 3. 模型身份未知 → 警示（不当匹配对待）。
    push(
        &mut cases,
        "identity_unknown",
        check_model_identity("fake-adapter-v1", None),
        Verdict::Warn,
    );
    // 4. 权限声明合规。
    push(
        &mut cases,
        "permission_ok",
        check_permission_declaration(&["read_files".to_owned(), "edit_files".to_owned()]),
        Verdict::Pass,
    );
    // 5. 权限声明含禁用能力 → 拒绝。
    push(
        &mut cases,
        "permission_violation",
        check_permission_declaration(&["read_files".to_owned(), "shell".to_owned()]),
        Verdict::Reject,
    );
    // 6. 认证失败 → 停止（不切换账号）。
    push(
        &mut cases,
        "auth_fail_stop",
        check_stop_conditions(&SimFrame::AuthFailed {
            provider: "minimax".to_owned(),
        }),
        Verdict::Stop,
    );
    // 7. 限流 → 停止（不改计费路线）。
    push(
        &mut cases,
        "rate_limit_stop",
        check_stop_conditions(&SimFrame::RateLimited {
            reason: "quota_exceeded".to_owned(),
        }),
        Verdict::Stop,
    );
    // 8. 诊断不泄露假凭据（canary 被清洗）。
    let redacted = redact_diagnostic("auth header sk-canary-123456 leaked check");
    let clean = !redacted.contains("sk-canary-") && redacted.contains("[已过滤]");
    cases.push((
        "diagnostics_redacted",
        if clean {
            Verdict::Pass
        } else {
            Verdict::Reject
        },
        format!(
            "{}：诊断输出 = {redacted:?}",
            if clean { "PASS" } else { "FAIL" }
        ),
    ));
    // 9. 低磁盘不领取。
    push(
        &mut cases,
        "low_disk_no_claim",
        check_disk_gate(100, 1024),
        Verdict::Stop,
    );
    // 10. 磁盘充足允许领取。
    push(
        &mut cases,
        "disk_ok_claim",
        check_disk_gate(2048, 1024),
        Verdict::Pass,
    );
    cases
}

/// JSON 概览（MCP/CLI 友好输出）。
pub fn cases_json(cases: &[(&'static str, Verdict, String)]) -> Value {
    json!({
        "schema_version": 1,
        "routes": CANDIDATE_ROUTES
            .iter()
            .map(|(id, enabled, requirements)| {
                json!({
                    "id": id, "enabled": enabled,
                    "status": if *enabled { "enabled" } else { "candidate-disabled" },
                    "requirements": requirements,
                })
            })
            .collect::<Vec<_>>(),
        "cases": cases
            .iter()
            .map(|(name, verdict, detail)| {
                json!({"name": name, "verdict": format!("{verdict:?}"), "detail": detail})
            })
            .collect::<Vec<_>>(),
        "all_pass": cases.iter().all(|(_, v, d)| {
            matches!(v, Verdict::Pass) || d.starts_with("PASS")
        }),
    })
}
