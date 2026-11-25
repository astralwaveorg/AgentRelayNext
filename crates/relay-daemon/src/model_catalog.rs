//! Versioned, read-only model catalog and deterministic role recommendation.
//!
//! This is deliberately small: it lists only routes with a verified local
//! implementation/review path. A catalog entry is not an account entitlement;
//! the user must still confirm the native plan and credentials in preview.

use serde_json::{json, Value};
#[cfg(test)]
use std::collections::BTreeSet;

pub const CATALOG_VERSION: u32 = 1;

fn entries() -> Value {
    json!([
        {
            "id":"minimax/MiniMax-M3.1-Flash",
            "aliases":[],
            "provider":"MiniMax",
            "tool":"Claude Code",
            "roles":["implement"],
            "capabilities":["read_files","edit_files"],
            "cost":"existing_subscription",
            "verified":true,
            "production":false,
            "production_ready":false,
            "evidence_level":"local_preview",
            "observed_at":"2026-11-24",
            "source":"本机真实路线预览与 Claude Code 受限写入夹具",
            "notes":"M Plan Credits 可能被使用；供应商扣费归属没有可靠回传，记 unverified。"
        },
        {
            "id":"opencode-go/deepseek-v4.1-flash",
            "aliases":["deepseek-v4.1-flash"],
            "provider":"OpenCode Go",
            "tool":"OpenCode",
            "roles":["review"],
            "capabilities":["read_files"],
            "cost":"existing_subscription",
            "verified":true,
            "production":false,
            "production_ready":false,
            "evidence_level":"local_preview",
            "observed_at":"2026-11-24",
            "source":"本机 OpenCode Go 网关与供应商响应核对",
            "notes":"必须核对 provider_reported_source 为 go/go-plus 且余额扣费为 0。"
        },
        {
            "id":"offline/fake-implementer-v1",
            "aliases":[],
            "provider":"local fixture",
            "tool":"local fake adapter",
            "roles":["implement"],
            "capabilities":["read_files","edit_files"],
            "cost":"free_tier",
            "verified":true,
            "production":false,
            "source":"仓库内 mock/catalog",
            "notes":"仅用于离线演示和回归，不证明真实权限、模型身份或套餐。"
        },
        {
            "id":"offline/fake-reviewer-v1",
            "aliases":[],
            "provider":"local fixture",
            "tool":"local fake adapter",
            "roles":["review"],
            "capabilities":["read_files"],
            "cost":"free_tier",
            "verified":true,
            "production":false,
            "source":"仓库内 mock/catalog",
            "notes":"仅用于离线演示和回归，不证明独立真实评审。"
        }
    ])
}

pub fn catalog() -> Value {
    json!({
        "schema_version": 1,
        "catalog_version": CATALOG_VERSION,
        "updated_at": "2026-11-24",
        "entries": entries(),
        "note": "目录只表示已验证的 Relay 路线，不表示账户余额、登录状态或任意型号支持。"
    })
}

/// Normalized model identity shared by the catalog and independence checks:
/// lower-case, provider prefix removed, catalog aliases folded onto the
/// canonical id. Two entries with equal normalized ids are the same model.
pub fn normalize_model(model: &str) -> String {
    let mut candidate = model.trim().to_ascii_lowercase();
    if let Some((_provider, tail)) = candidate.rsplit_once('/') {
        candidate = tail.to_owned();
    }
    if let Some(entries) = entries().as_array() {
        for entry in entries {
            let mut names: Vec<String> = Vec::new();
            if let Some(id) = entry["id"].as_str() {
                names.push(id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase());
            }
            if let Some(aliases) = entry["aliases"].as_array() {
                for alias in aliases.iter().filter_map(|a| a.as_str()) {
                    names.push(
                        alias
                            .rsplit('/')
                            .next()
                            .unwrap_or(alias)
                            .to_ascii_lowercase(),
                    );
                }
            }
            if names.contains(&candidate) {
                if let Some(id) = entry["id"].as_str() {
                    return id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
                }
            }
        }
    }
    candidate
}

fn entry_id(entry: &Value) -> &str {
    entry["id"].as_str().unwrap_or_default()
}

#[cfg(test)]
fn aliases(entry: &Value) -> BTreeSet<&str> {
    let mut set = BTreeSet::new();
    if let Some(id) = entry["id"].as_str() {
        set.insert(id);
    }
    if let Some(items) = entry["aliases"].as_array() {
        for item in items {
            if let Some(alias) = item.as_str() {
                set.insert(alias);
            }
        }
    }
    set
}

/// Deterministic recommendation: filter verified production entries by role,
/// then require different tools and non-overlapping normalized model IDs.
/// Aliases are deliberately treated as the same model.
pub fn recommend() -> Value {
    let all = entries();
    let list: Vec<&Value> = all
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let implements: Vec<&Value> = list
        .iter()
        .copied()
        .filter(|e| {
            e["verified"] == true
                && e["roles"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == "implement"))
                && e["capabilities"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|v| v == "edit_files"))
        })
        .collect();
    let reviews: Vec<&Value> = list
        .iter()
        .copied()
        .filter(|e| {
            e["verified"] == true
                && e["roles"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == "review"))
                && e["capabilities"]
                    .as_array()
                    .is_some_and(|c| c.len() == 1 && c[0] == "read_files")
        })
        .collect();
    for implement in &implements {
        for review in &reviews {
            if implement["tool"] == review["tool"] {
                continue;
            }
            if normalize_model(entry_id(implement)) == normalize_model(entry_id(review)) {
                continue;
            }
            return json!({
                "schema_version": 1,
                "status": "recommended",
                "template": "implement_review",
                "implement": {"id": entry_id(implement), "tool": implement["tool"],
                    "provider": implement["provider"], "cost": implement["cost"],
                    "verified": implement["verified"], "notes": implement["notes"]},
                "review": {"id": entry_id(review), "tool": review["tool"],
                    "provider": review["provider"], "cost": review["cost"],
                    "verified": review["verified"], "notes": review["notes"]},
                "independence": {"different_tool": true, "different_model": true},
                "reason": "实现者可写、评审者只读，工具和归一化模型身份均不同；真实账户证据完成前为预览推荐。",
                "production_ready": false,
                "preview_only": true,
                "max_rework": 0,
                "fallback": false
            });
        }
    }
    json!({
        "schema_version": 1,
        "status": "no_verified_pair",
        "production_ready": false,
        "preview_only": true,
        "reason": "没有同时满足实现/只读评审、工具与模型独立性的已验证组合。",
        "action": "check_native_plans"
    })
}

pub fn cli(args: &[String]) -> Result<(), u8> {
    match args {
        [] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&catalog()).unwrap_or_default()
            );
            Ok(())
        }
        [sub] if sub == "list" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&catalog()).unwrap_or_default()
            );
            Ok(())
        }
        [sub] if sub == "recommend" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&recommend()).unwrap_or_default()
            );
            Ok(())
        }
        _ => {
            eprintln!("用法：relayctl catalog [list|recommend]");
            Err(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_is_deterministic_and_independent() {
        let first = recommend();
        assert_eq!(first, recommend());
        assert_eq!(first["status"], "recommended");
        assert_ne!(first["implement"]["tool"], first["review"]["tool"]);
        assert_ne!(first["implement"]["id"], first["review"]["id"]);
        assert_eq!(first["template"], "implement_review");
        assert_eq!(first["production_ready"], false);
        assert_eq!(first["preview_only"], true);
        assert_eq!(first["max_rework"], 0);
    }

    #[test]
    fn aliases_do_not_create_independence() {
        let entry = json!({"id":"model-a","aliases":["alias-a"],"tool":"tool-a"});
        let alias = json!({"id":"alias-a","aliases":[],"tool":"tool-b"});
        let a = aliases(&entry);
        let b = aliases(&alias);
        assert!(a.intersection(&b).next().is_some());
        assert_eq!(
            normalize_model("zhipuai/glm-5.3-flash"),
            normalize_model("glm-5.3-flash")
        );
        assert_eq!(
            normalize_model("opencode-go/deepseek-v4.1-flash"),
            "deepseek-v4.1-flash"
        );
        assert_ne!(
            normalize_model("deepseek-v4.1-flash"),
            normalize_model("MiniMax-M3.1-Flash")
        );
    }

    #[test]
    fn offline_entries_are_not_recommended_for_production() {
        let all = catalog();
        let recommendation = recommend().to_string();
        assert!(!recommendation.contains("offline/fake"));
        assert!(all["entries"]
            .as_array()
            .is_some_and(|items| { items.iter().any(|item| item["production"] == false) }));
    }
}
