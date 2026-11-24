//! relay_plan / relay_submit / relay_status 的最小 DTO（04 字段约束、
//! 14 契约样本）。字段校验在此完成：长度、条数与去重；未知字段与
//! 缺失字段由 serde deny_unknown_fields 与必填字段处理。

use serde::Deserialize;
use serde_json::Value;

use crate::error::ProtocolError;

/// 04：goal ≤ 1 KiB。
pub const MAX_GOAL_BYTES: usize = 1024;
/// 04：brief ≤ 16 KiB。
pub const MAX_BRIEF_BYTES: usize = 16 * 1024;
/// 04：write_scope 最多 64 条。
pub const MAX_WRITE_SCOPE_ENTRIES: usize = 64;
/// 04：write_scope 单条 ≤ 240 B。
pub const MAX_WRITE_SCOPE_ENTRY_BYTES: usize = 240;
/// 04：rule IDs 1–8 条且去重。
pub const MAX_RULE_IDS: usize = 8;

fn validate_plan_fields(
    goal: &str,
    brief: &str,
    write_scope: &[String],
    verification_rule_ids: &[String],
) -> Result<(), ProtocolError> {
    if goal.len() > MAX_GOAL_BYTES {
        return Err(ProtocolError::Constraint {
            detail: format!("goal {} 字节超过 1 KiB 上限", goal.len()),
        });
    }
    if brief.len() > MAX_BRIEF_BYTES {
        return Err(ProtocolError::Constraint {
            detail: format!("brief {} 字节超过 16 KiB 上限", brief.len()),
        });
    }
    if write_scope.len() > MAX_WRITE_SCOPE_ENTRIES {
        return Err(ProtocolError::Constraint {
            detail: format!("write_scope {} 条超过 64 条上限", write_scope.len()),
        });
    }
    for entry in write_scope {
        if entry.len() > MAX_WRITE_SCOPE_ENTRY_BYTES {
            return Err(ProtocolError::Constraint {
                detail: format!("write_scope 单条 {} 字节超过 240 B 上限", entry.len()),
            });
        }
    }
    if verification_rule_ids.is_empty() {
        return Err(ProtocolError::Constraint {
            detail: "verification_rule_ids 必须为 1–8 条且去重".into(),
        });
    }
    if verification_rule_ids.len() > MAX_RULE_IDS {
        return Err(ProtocolError::Constraint {
            detail: format!(
                "verification_rule_ids {} 条超过 8 条上限",
                verification_rule_ids.len()
            ),
        });
    }
    let mut seen: Vec<&str> = Vec::new();
    for id in verification_rule_ids {
        if seen.contains(&id.as_str()) {
            return Err(ProtocolError::Constraint {
                detail: format!("verification_rule_ids 含重复 ID {id}"),
            });
        }
        seen.push(id);
    }
    Ok(())
}

/// relay_plan 请求（04 必填字段；可选 base_oid）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub project_id: String,
    pub goal: String,
    pub brief: String,
    pub write_scope: Vec<String>,
    pub profile_id: String,
    pub verification_rule_ids: Vec<String>,
    #[serde(default)]
    pub base_oid: Option<String>,
}

impl PlanRequest {
    /// 从 params JSON 值解析并执行 04 字段约束校验。
    pub fn from_params(params: &Value) -> Result<PlanRequest, ProtocolError> {
        let request: PlanRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.project_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "project_id".into(),
            });
        }
        if request.profile_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "profile_id".into(),
            });
        }
        validate_plan_fields(
            &request.goal,
            &request.brief,
            &request.write_scope,
            &request.verification_rule_ids,
        )?;
        Ok(request)
    }
}

/// relay_submit 请求：plan_id 与全局唯一 request_id。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub plan_id: String,
    pub request_id: String,
}

impl SubmitRequest {
    pub fn from_params(params: &Value) -> Result<SubmitRequest, ProtocolError> {
        let request: SubmitRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.plan_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "plan_id".into(),
            });
        }
        if !is_uuid(&request.request_id) {
            return Err(ProtocolError::Constraint {
                detail: "request_id 必须是 UUID".into(),
            });
        }
        Ok(request)
    }
}

/// UUID 形状校验：8-4-4-4-12 十六进制段，连字符位置固定；
/// 十六进制大小写不敏感。
fn is_uuid(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, byte) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_digit()
                    && !(b'a'..=b'f').contains(byte)
                    && !(b'A'..=b'F').contains(byte)
                {
                    return false;
                }
            }
        }
    }
    true
}

/// relay_status 请求：task_id。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    pub task_id: String,
}

impl StatusRequest {
    pub fn from_params(params: &Value) -> Result<StatusRequest, ProtocolError> {
        let request: StatusRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.task_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "task_id".into(),
            });
        }
        Ok(request)
    }
}

/// submit/status 响应中的最小任务对象（14 成功结果最少形态）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskRef {
    pub id: String,
    pub state: String,
    pub revision: u64,
    pub active_attempt: u32,
}

/// submit 响应（04 成功结果最少形态）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubmitResult {
    pub schema_version: u32,
    pub task: TaskRef,
}

impl SubmitResult {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"schema_version\":1}".to_owned())
    }
}

/// status 响应最小形态：task 对象与外层 revision（14）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatusResult {
    pub schema_version: u32,
    pub task: TaskRef,
    pub revision: u64,
}

impl StatusResult {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"schema_version\":1}".to_owned())
    }
}

// ---- M1 业务 DTO：cancel/retry/review/list/artifact/events（04 表） ----

/// relay_cancel：已活动先回 cancelling；终态兑现第一个有效操作。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub task_id: String,
    pub expected_revision: u64,
    pub request_id: String,
}

impl CancelRequest {
    pub fn from_params(params: &Value) -> Result<CancelRequest, ProtocolError> {
        let request: CancelRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.task_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "task_id".into(),
            });
        }
        if request.expected_revision == 0 {
            return Err(ProtocolError::Constraint {
                detail: "expected_revision 必须为正整数".into(),
            });
        }
        if !is_uuid(&request.request_id) {
            return Err(ProtocolError::Constraint {
                detail: "request_id 必须是 UUID".into(),
            });
        }
        Ok(request)
    }
}

/// relay_retry：新 plan 同条件；中断无法证实进程退出时不创建 attempt。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRequest {
    pub task_id: String,
    pub plan_id: String,
    pub expected_revision: u64,
    pub request_id: String,
}

impl RetryRequest {
    pub fn from_params(params: &Value) -> Result<RetryRequest, ProtocolError> {
        let request: RetryRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.task_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "task_id".into(),
            });
        }
        if request.plan_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "plan_id".into(),
            });
        }
        if request.expected_revision == 0 {
            return Err(ProtocolError::Constraint {
                detail: "expected_revision 必须为正整数".into(),
            });
        }
        if !is_uuid(&request.request_id) {
            return Err(ProtocolError::Constraint {
                detail: "request_id 必须是 UUID".into(),
            });
        }
        Ok(request)
    }
}

/// relay_review：必须在 awaiting_review，校验本机摘要与门禁。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub task_id: String,
    pub attempt_no: u32,
    pub expected_revision: u64,
    pub request_id: String,
    /// "accept" / "reject"。
    pub verdict: String,
    /// 64 个小写十六进制字符（04：sha256 仅指制品哈希）。
    pub diff_sha256: String,
    pub test_sha256: String,
    /// ≤ 2 KiB（04）。
    #[serde(default)]
    pub note: Option<String>,
}

impl ReviewRequest {
    pub fn from_params(params: &Value) -> Result<ReviewRequest, ProtocolError> {
        let request: ReviewRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.task_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "task_id".into(),
            });
        }
        if request.attempt_no == 0 || request.expected_revision == 0 {
            return Err(ProtocolError::Constraint {
                detail: "attempt_no 与 expected_revision 必须为正整数".into(),
            });
        }
        if !is_uuid(&request.request_id) {
            return Err(ProtocolError::Constraint {
                detail: "request_id 必须是 UUID".into(),
            });
        }
        if request.verdict != "accept" && request.verdict != "reject" {
            return Err(ProtocolError::Constraint {
                detail: "verdict 只允许 accept 或 reject".into(),
            });
        }
        for (field, value) in [
            ("diff_sha256", &request.diff_sha256),
            ("test_sha256", &request.test_sha256),
        ] {
            let b = value.as_bytes();
            if b.len() != 64
                || !b
                    .iter()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
            {
                return Err(ProtocolError::Constraint {
                    detail: format!("{field} 必须是 64 个小写十六进制字符"),
                });
            }
        }
        if let Some(note) = &request.note {
            if note.len() > 2 * 1024 {
                return Err(ProtocolError::Constraint {
                    detail: "note 超 2 KiB".into(),
                });
            }
        }
        Ok(request)
    }
}

/// relay_list：默认 20，上限 50，稳定分页游标绑定查询范围。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub project_id: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ListRequest {
    pub fn from_params(params: &Value) -> Result<ListRequest, ProtocolError> {
        let request: ListRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.project_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "project_id".into(),
            });
        }
        if let Some(limit) = request.limit {
            if limit == 0 || limit > 50 {
                return Err(ProtocolError::Constraint {
                    detail: "limit 须在 1..=50（默认 20）".into(),
                });
            }
        }
        Ok(request)
    }
}

/// relay_artifact：kind 仅 manifest/handoff/diff/test_receipt；有界切片。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRequest {
    pub task_id: String,
    pub attempt_no: u32,
    pub kind: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ArtifactRequest {
    pub const KINDS: [&'static str; 4] = ["manifest", "handoff", "diff", "test_receipt"];

    pub fn from_params(params: &Value) -> Result<ArtifactRequest, ProtocolError> {
        let request: ArtifactRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if request.task_id.is_empty() {
            return Err(ProtocolError::MissingField {
                field: "task_id".into(),
            });
        }
        if request.attempt_no == 0 {
            return Err(ProtocolError::Constraint {
                detail: "attempt_no 必须为正整数".into(),
            });
        }
        if !Self::KINDS.contains(&request.kind.as_str()) {
            return Err(ProtocolError::Constraint {
                detail: "kind 仅 manifest、handoff、diff、test_receipt".into(),
            });
        }
        if request.limit.is_some_and(|l| l == 0 || l > 32 * 1024) {
            return Err(ProtocolError::Constraint {
                detail: "limit 须在 1..=32768（默认/最大 32 KiB）".into(),
            });
        }
        Ok(request)
    }
}

/// events.list 短轮询（M1 CLI）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsRequest {
    #[serde(default)]
    pub after_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl EventsRequest {
    pub fn from_params(params: &Value) -> Result<EventsRequest, ProtocolError> {
        let request: EventsRequest =
            serde_json::from_value(params.clone()).map_err(|e| crate::error::map_serde(&e))?;
        if let Some(limit) = request.limit {
            if limit == 0 || limit > 100 {
                return Err(ProtocolError::Constraint {
                    detail: "limit 须在 1..=100".into(),
                });
            }
        }
        Ok(request)
    }
}
