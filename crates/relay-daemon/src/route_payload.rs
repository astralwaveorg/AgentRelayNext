//! Shared, data-only boundary for a project review sent to a model adapter.
use serde_json::{json, Value};

pub(crate) fn review_candidate(
    goal: &str,
    patch: &str,
    receipt: &str,
) -> Result<Value, &'static str> {
    let parsed = relay_protocol::frame::decode_json_value(receipt).map_err(|_| "REVIEW_RECEIPT")?;
    let diff_sha = relay_git::sha256_hex(patch.as_bytes());
    if patch.is_empty()
        || parsed["schema_version"] != 1
        || parsed["status"] != "pass"
        || parsed["source_modified"] != false
        || parsed["diff_sha256"] != diff_sha
        || parsed["post_diff_sha256"] != diff_sha
    {
        return Err("REVIEW_RECEIPT_NOT_BOUND");
    }
    let entries = parsed["rules"]
        .as_array()
        .filter(|rules| !rules.is_empty() && rules.len() <= 8)
        .ok_or("REVIEW_RECEIPT_RULES")?;
    let mut rules = Vec::with_capacity(entries.len());
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let id = entry["rule_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 64 && !id.chars().any(char::is_control))
            .ok_or("REVIEW_RECEIPT_RULES")?;
        if entry["status"] != "pass" || !seen.insert(id) {
            return Err("REVIEW_RECEIPT_RULES");
        }
        rules.push(json!({"rule_id":id,"status":"pass"}));
    }
    // Only these fields may reach a reviewer. The exact local receipt is still
    // hashed so that review, manual acceptance and the artifact remain bound.
    Ok(json!({
        "goal":goal,
        "diff":patch,
        "receipt":{"status":"pass","rules":rules},
        "candidate_diff_sha256":diff_sha,
        "test_receipt_sha256":relay_git::sha256_hex(receipt.as_bytes())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_candidate_and_minimal_test_evidence_leave_the_workflow() {
        let diff = "+safe diff\n";
        let sha = relay_git::sha256_hex(diff.as_bytes());
        let receipt = json!({
            "schema_version":1,"status":"pass","source_modified":false,
            "diff_sha256":sha,"post_diff_sha256":sha,
            "rules":[{"rule_id":"check","status":"pass","output_excerpt":"SECRET",
                      "display_argv":["/Users/local/check","--key=secret"]}]
        })
        .to_string();
        let outgoing = review_candidate("goal", diff, &receipt).unwrap();
        assert_eq!(
            outgoing["receipt"],
            json!({"status":"pass","rules":[{"rule_id":"check","status":"pass"}]})
        );
        assert_eq!(
            outgoing["test_receipt_sha256"],
            relay_git::sha256_hex(receipt.as_bytes())
        );
        let encoded = outgoing.to_string();
        assert!(
            !encoded.contains("SECRET")
                && !encoded.contains("--key")
                && !encoded.contains("/Users/")
        );
        assert!(review_candidate("goal", "+different", &receipt).is_err());
    }
}
