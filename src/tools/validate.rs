//! Argument validation. One rule per fn; each returns an
//! [`InvalidArgument`](crate::error::ChittaError::InvalidArgument) with a
//! populated `constraint` + `next_action` on failure (Principle 8).

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::error::{ChittaError, Result};

/// Profile: 1-128 chars, `[a-zA-Z0-9_-]+`.
pub fn profile(tool: &'static str, value: &str) -> Result<()> {
    let char_count = value.chars().count();
    let len_ok = (1..=128).contains(&char_count);
    let chars_ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !len_ok || !chars_ok {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "profile".to_string(),
            constraint: "1-128 chars, [a-zA-Z0-9_-]+ only".to_string(),
            received: Some(json!(value)),
            next_action:
                "Pass a non-empty profile of ≤128 ASCII letters, digits, underscores, or hyphens."
                    .to_string(),
        });
    }
    Ok(())
}

/// 4 MB byte-length cap — cheap O(1) defense-in-depth before tokenization.
pub const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// Content byte length: at most 4 MB (`MAX_CONTENT_BYTES`).
///
/// This is a cheap pre-tokenization gate. The token-length bound is enforced
/// separately inside `embed`.
pub fn content_byte_length(tool: &'static str, value: &str) -> Result<()> {
    if value.len() > MAX_CONTENT_BYTES {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "content".to_string(),
            constraint: format!("content must be at most {} bytes", MAX_CONTENT_BYTES),
            received: Some(json!(value.len())),
            next_action: format!(
                "Reduce content size. Current: {} bytes, limit: {} bytes. \
                 Split into multiple memories if needed.",
                value.len(),
                MAX_CONTENT_BYTES
            ),
        });
    }
    Ok(())
}

/// Content: non-empty. Token-length bound is enforced inside `embed`.
pub fn content_non_empty(tool: &'static str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "content".to_string(),
            constraint: "length >= 1".to_string(),
            received: Some(json!("")),
            next_action: "Pass non-empty content.".to_string(),
        });
    }
    Ok(())
}

/// Idempotency key: 1-128 chars, no control characters.
pub fn idempotency_key(tool: &'static str, value: &str) -> Result<()> {
    let char_count = value.chars().count();
    let len_ok = (1..=128).contains(&char_count);
    let no_control = value.chars().all(|c| !c.is_control());
    if !len_ok || !no_control {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "idempotency_key".to_string(),
            constraint: "1-128 chars, no control characters".to_string(),
            received: Some(json!(value)),
            next_action:
                "Pass a 1-128 character idempotency_key with no control characters (e.g. a UUID \
                 or a client-stable hash)."
                    .to_string(),
        });
    }
    Ok(())
}

/// Event time: `>= 1970-01-01T00:00:00Z` and `<= now + 365 days`.
pub fn event_time(tool: &'static str, value: DateTime<Utc>) -> Result<()> {
    let epoch = Utc.timestamp_opt(0, 0).single().expect("epoch is valid");
    let upper = Utc::now() + Duration::days(365);
    if value < epoch {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "event_time".to_string(),
            constraint: "ISO-8601 timestamp >= 1970-01-01T00:00:00Z".to_string(),
            received: Some(json!(value.to_rfc3339())),
            next_action:
                "Pass event_time >= 1970-01-01T00:00:00Z, or omit to default to record_time."
                    .to_string(),
        });
    }
    if value > upper {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "event_time".to_string(),
            constraint: "ISO-8601 timestamp <= now + 365 days".to_string(),
            received: Some(json!(value.to_rfc3339())),
            next_action:
                "Pass event_time within one year of now, or omit to default to record_time."
                    .to_string(),
        });
    }
    Ok(())
}

/// Tags: up to 32 entries, each 1-64 chars.
pub fn tags(tool: &'static str, values: &[String]) -> Result<()> {
    if values.len() > 32 {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "tags".to_string(),
            constraint: "at most 32 tags".to_string(),
            received: Some(json!({ "count": values.len() })),
            next_action: "Trim the tag list to at most 32 entries.".to_string(),
        });
    }
    for (i, t) in values.iter().enumerate() {
        let char_count = t.chars().count();
        if char_count == 0 || char_count > 64 {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "tags".to_string(),
                constraint: "each tag 1-64 chars".to_string(),
                received: Some(json!({ "index": i, "length": char_count })),
                next_action: "Ensure every tag is between 1 and 64 characters.".to_string(),
            });
        }
    }
    Ok(())
}

/// Upper bound on `k` for search. Chosen so a single response cannot dwarf
/// the agent's context window even with long snippets; callers that need more
/// results should page via tag or time filters.
pub const MAX_K: i64 = 200;

/// `k` for search: integer in `[1, MAX_K]`.
pub fn k(tool: &'static str, value: i64) -> Result<()> {
    if !(1..=MAX_K).contains(&value) {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "k".to_string(),
            constraint: format!("integer in [1, {MAX_K}]"),
            received: Some(json!(value)),
            next_action: format!("Pass k between 1 and {MAX_K} (default is 10)."),
        });
    }
    Ok(())
}

/// Cosine similarity floor: finite float in `[0.0, 1.0]`.
pub fn min_similarity(tool: &'static str, value: f32) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "min_similarity".to_string(),
            constraint: "finite float in [0.0, 1.0]".to_string(),
            received: Some(json!(value)),
            next_action: "Pass min_similarity between 0.0 and 1.0 inclusive.".to_string(),
        });
    }
    Ok(())
}

/// Token budget: positive.
pub fn max_tokens(tool: &'static str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "max_tokens".to_string(),
            constraint: "> 0".to_string(),
            received: Some(json!(value)),
            next_action: "Pass a positive max_tokens, or omit to disable the budget.".to_string(),
        });
    }
    Ok(())
}

pub const VALID_MEMORY_TYPES: &[&str] = &[
    "observation",
    "episode",
    "decision",
    "trait",
    "value",
    "pattern",
    "preference",
    "mental_model",
];

pub fn memory_type(tool: &'static str, value: &str) -> Result<()> {
    if !VALID_MEMORY_TYPES.contains(&value) {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "memory_type".to_string(),
            constraint: format!("one of: {}", VALID_MEMORY_TYPES.join(", ")),
            received: Some(json!(value)),
            next_action: format!("Use one of: {}", VALID_MEMORY_TYPES.join(", ")),
        });
    }
    Ok(())
}

pub fn memory_types(tool: &'static str, values: &[String]) -> Result<()> {
    for v in values {
        memory_type(tool, v)?;
    }
    Ok(())
}

/// Derivation input from the caller (before UUID parsing).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct DerivationInput {
    /// UUID of the source memory this derivation links to.
    pub source_id: String,
    /// Type of derivation (e.g. "synthesised_from", "supersedes").
    pub derivation_type: String,
}

pub fn episode_derivations(
    tool: &'static str,
    memory_type: &str,
    derivations: &Option<Vec<DerivationInput>>,
) -> Result<()> {
    if memory_type != "episode" {
        return Ok(());
    }
    match derivations {
        None => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "derivations".to_string(),
                constraint: "episode memory requires at least one derivation".to_string(),
                received: Some(json!(null)),
                next_action: "episode memory requires at least one entry in derivations \
                    linking to source observations. Either supply derivations, \
                    or use memory_type=observation."
                    .to_string(),
            });
        }
        Some(v) if v.is_empty() => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "derivations".to_string(),
                constraint: "episode memory requires at least one derivation".to_string(),
                received: Some(json!([])),
                next_action: "episode memory requires at least one entry in derivations \
                    linking to source observations. Either supply derivations, \
                    or use memory_type=observation."
                    .to_string(),
            });
        }
        Some(v) => {
            for (i, d) in v.iter().enumerate() {
                parse_uuid(tool, "derivations[].source_id", &d.source_id).map_err(|_| {
                    ChittaError::InvalidArgument {
                        tool,
                        argument: "derivations".to_string(),
                        constraint: "source_id must be a valid UUID".to_string(),
                        received: Some(json!({"index": i, "source_id": &d.source_id})),
                        next_action: "Pass a valid UUID for source_id.".to_string(),
                    }
                })?;
                if d.derivation_type.is_empty() {
                    return Err(ChittaError::InvalidArgument {
                        tool,
                        argument: "derivations".to_string(),
                        constraint: "derivation_type must be non-empty".to_string(),
                        received: Some(json!({"index": i, "derivation_type": ""})),
                        next_action: "Pass a non-empty derivation_type (e.g. \"synthesised_from\").".to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

pub const VALID_REF_KINDS: &[&str] = &["file", "commit", "yojana_task", "memory", "url", "session"];

pub fn ref_filter(tool: &'static str, rf: &super::search::RefFilter) -> Result<()> {
    if !VALID_REF_KINDS.contains(&rf.kind.as_str()) {
        return Err(ChittaError::InvalidArgument {
            tool,
            argument: "ref_filter.kind".to_string(),
            constraint: format!("kind must be one of: {}", VALID_REF_KINDS.join(", ")),
            received: Some(json!({"kind": &rf.kind})),
            next_action: format!("Use a valid kind: {}", VALID_REF_KINDS.join(", ")),
        });
    }
    if let Some(ref val) = rf.ref_value {
        if val.is_empty() {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "ref_filter.ref".to_string(),
                constraint: "ref must be non-empty when provided".to_string(),
                received: Some(json!({"ref": ""})),
                next_action: "Provide a non-empty ref value or omit the field.".to_string(),
            });
        }
    }
    Ok(())
}

/// External refs: JSON array of `{"kind": "<type>", "ref": "<value>"}`.
pub fn external_refs(tool: &'static str, value: &serde_json::Value) -> Result<()> {
    let arr = value
        .as_array()
        .ok_or_else(|| ChittaError::InvalidArgument {
            tool,
            argument: "external_refs".to_string(),
            constraint: "must be a JSON array".to_string(),
            received: Some(json!({"type": value_type_name(value)})),
            next_action:
                r#"Pass external_refs as an array: [{"kind": "file", "ref": "path/to/file"}]."#
                    .to_string(),
        })?;
    for (i, entry) in arr.iter().enumerate() {
        let obj = entry
            .as_object()
            .ok_or_else(|| ChittaError::InvalidArgument {
                tool,
                argument: "external_refs".to_string(),
                constraint:
                    "each element must be an object with \"kind\" and \"ref\" string fields"
                        .to_string(),
                received: Some(json!({"index": i, "type": value_type_name(entry)})),
                next_action: r#"Each element must be {"kind": "<type>", "ref": "<value>"}."#
                    .to_string(),
            })?;
        let kind = obj.get("kind").and_then(|v| v.as_str()).ok_or_else(|| {
            ChittaError::InvalidArgument {
                tool,
                argument: "external_refs".to_string(),
                constraint: "each element must have a string \"kind\" field".to_string(),
                received: Some(json!({"index": i})),
                next_action: format!(
                    "Add a \"kind\" field. Valid kinds: {}",
                    VALID_REF_KINDS.join(", ")
                ),
            }
        })?;
        if !VALID_REF_KINDS.contains(&kind) {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "external_refs".to_string(),
                constraint: format!("kind must be one of: {}", VALID_REF_KINDS.join(", ")),
                received: Some(json!({"index": i, "kind": kind})),
                next_action: format!("Use a valid kind: {}", VALID_REF_KINDS.join(", ")),
            });
        }
        let ref_val = obj
            .get("ref")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ChittaError::InvalidArgument {
                tool,
                argument: "external_refs".to_string(),
                constraint: "each element must have a non-empty string \"ref\" field".to_string(),
                received: Some(json!({"index": i})),
                next_action: r#"Add a "ref" field with the reference value (e.g. a file path, URL, or UUID)."#
                    .to_string(),
            })?;
        if ref_val.is_empty() {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "external_refs".to_string(),
                constraint: "\"ref\" must be non-empty".to_string(),
                received: Some(json!({"index": i, "ref": ""})),
                next_action: "Provide a non-empty ref value.".to_string(),
            });
        }
    }
    Ok(())
}

fn value_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Parse a UUID argument, translating parse errors to a populated
/// `InvalidArgument`.
pub fn parse_uuid(tool: &'static str, argument: &'static str, value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|e| ChittaError::InvalidArgument {
        tool,
        argument: argument.to_string(),
        constraint: "valid UUID".to_string(),
        received: Some(json!(value)),
        next_action: format!("Pass a valid UUID string. Parse error: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_rules() {
        assert!(profile("t", "default").is_ok());
        assert!(profile("t", "alpha_beta-1").is_ok());
        assert!(profile("t", "").is_err());
        assert!(profile("t", "has space").is_err());
        assert!(profile("t", &"a".repeat(129)).is_err());
    }

    #[test]
    fn idempotency_key_rules() {
        assert!(idempotency_key("t", "key-1").is_ok());
        assert!(idempotency_key("t", "").is_err());
        assert!(idempotency_key("t", "has\ncontrol").is_err());
        // 128 four-byte code points = 512 bytes, must be accepted (was rejected
        // when we measured bytes instead of chars).
        let multibyte: String = "😀".repeat(128);
        assert_eq!(multibyte.chars().count(), 128);
        assert!(idempotency_key("t", &multibyte).is_ok());
        let too_long: String = "😀".repeat(129);
        assert!(idempotency_key("t", &too_long).is_err());
    }

    #[test]
    fn k_rules() {
        assert!(k("t", 1).is_ok());
        assert!(k("t", MAX_K).is_ok());
        assert!(k("t", 0).is_err());
        assert!(k("t", -5).is_err());
        assert!(k("t", MAX_K + 1).is_err());
    }

    #[test]
    fn min_similarity_rules() {
        assert!(min_similarity("t", 0.0).is_ok());
        assert!(min_similarity("t", 0.5).is_ok());
        assert!(min_similarity("t", 1.0).is_ok());
        assert!(min_similarity("t", -0.01).is_err());
        assert!(min_similarity("t", 1.01).is_err());
        assert!(min_similarity("t", f32::NAN).is_err());
        assert!(min_similarity("t", f32::INFINITY).is_err());
    }

    #[test]
    fn max_tokens_rules() {
        assert!(max_tokens("t", 1).is_ok());
        assert!(max_tokens("t", u64::MAX).is_ok());
        assert!(max_tokens("t", 0).is_err());
    }

    #[test]
    fn event_time_rules() {
        let pre_epoch = Utc.with_ymd_and_hms(1969, 6, 20, 0, 0, 0).single().unwrap();
        assert!(event_time("t", pre_epoch).is_err());
        let now = Utc::now();
        assert!(event_time("t", now).is_ok());
        let too_far = now + Duration::days(400);
        assert!(event_time("t", too_far).is_err());
    }

    #[test]
    fn tag_rules() {
        assert!(tags("t", &[]).is_ok());
        assert!(tags("t", &["a".to_string(), "b".to_string()]).is_ok());
        let too_many: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
        assert!(tags("t", &too_many).is_err());
        assert!(tags("t", &["".to_string()]).is_err());
        assert!(tags("t", &["x".repeat(65)]).is_err());
    }

    #[test]
    fn content_byte_length_accepts_normal_input() {
        let normal = "hello world";
        let result = content_byte_length("test_tool", normal);
        assert!(result.is_ok());
    }

    #[test]
    fn content_byte_length_rejects_huge_input() {
        let huge = "x".repeat(5 * 1024 * 1024); // 5 MB
        let result = content_byte_length("test_tool", &huge);
        assert!(result.is_err());
        // Confirm the error message surfaces the actual byte count.
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("content"),
            "error message should mention the argument: {err_msg}"
        );
    }

    #[test]
    fn content_byte_length_accepts_exactly_at_limit() {
        let at_limit = "x".repeat(MAX_CONTENT_BYTES);
        assert!(content_byte_length("test_tool", &at_limit).is_ok());
    }

    #[test]
    fn content_byte_length_rejects_one_byte_over_limit() {
        let over = "x".repeat(MAX_CONTENT_BYTES + 1);
        assert!(content_byte_length("test_tool", &over).is_err());
    }

    #[test]
    fn memory_type_rules() {
        for valid in VALID_MEMORY_TYPES {
            assert!(memory_type("t", valid).is_ok(), "{valid} should be valid");
        }
        assert!(memory_type("t", "bogus").is_err());
        assert!(memory_type("t", "Memory").is_err());
        assert!(memory_type("t", "").is_err());
    }

    #[test]
    fn memory_types_rules() {
        assert!(memory_types("t", &["observation".into(), "decision".into()]).is_ok());
        assert!(memory_types("t", &["observation".into(), "bogus".into()]).is_err());
        assert!(memory_types("t", &[]).is_ok());
    }

    #[test]
    fn external_refs_valid() {
        let v = json!([{"kind": "file", "ref": "src/main.rs"}]);
        assert!(external_refs("t", &v).is_ok());

        let multi = json!([
            {"kind": "file", "ref": "a.rs"},
            {"kind": "commit", "ref": "abc123"},
            {"kind": "url", "ref": "https://example.com"},
        ]);
        assert!(external_refs("t", &multi).is_ok());

        assert!(external_refs("t", &json!([])).is_ok());
    }

    #[test]
    fn external_refs_not_array() {
        assert!(external_refs("t", &json!({"kind": "file", "ref": "x"})).is_err());
        assert!(external_refs("t", &json!("string")).is_err());
    }

    #[test]
    fn external_refs_bad_element() {
        assert!(external_refs("t", &json!(["not an object"])).is_err());
        assert!(external_refs("t", &json!([{"kind": "file"}])).is_err());
        assert!(external_refs("t", &json!([{"ref": "x"}])).is_err());
    }

    #[test]
    fn external_refs_invalid_kind() {
        assert!(external_refs("t", &json!([{"kind": "bogus", "ref": "x"}])).is_err());
    }

    #[test]
    fn external_refs_empty_ref() {
        assert!(external_refs("t", &json!([{"kind": "file", "ref": ""}])).is_err());
    }

    fn deriv(source_id: &str, dtype: &str) -> DerivationInput {
        DerivationInput {
            source_id: source_id.to_string(),
            derivation_type: dtype.to_string(),
        }
    }

    #[test]
    fn episode_derivations_rejects_none() {
        let r = episode_derivations("t", "episode", &None);
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("derivations"), "{msg}");
    }

    #[test]
    fn episode_derivations_rejects_empty() {
        let r = episode_derivations("t", "episode", &Some(vec![]));
        assert!(r.is_err());
    }

    #[test]
    fn episode_derivations_happy_path() {
        let valid_uuid = "019e0725-aab3-7160-905b-a150603d16d9";
        let derivs = Some(vec![deriv(valid_uuid, "synthesised_from")]);
        assert!(episode_derivations("t", "episode", &derivs).is_ok());
    }

    #[test]
    fn episode_derivations_multiple() {
        let u1 = "019e0725-aab3-7160-905b-a150603d16d9";
        let u2 = "019e0725-aab3-7160-905b-a150603d16da";
        let derivs = Some(vec![
            deriv(u1, "synthesised_from"),
            deriv(u2, "synthesised_from"),
        ]);
        assert!(episode_derivations("t", "episode", &derivs).is_ok());
    }

    #[test]
    fn episode_derivations_invalid_uuid() {
        let derivs = Some(vec![deriv("not-a-uuid", "synthesised_from")]);
        assert!(episode_derivations("t", "episode", &derivs).is_err());
    }

    #[test]
    fn episode_derivations_empty_type() {
        let valid_uuid = "019e0725-aab3-7160-905b-a150603d16d9";
        let derivs = Some(vec![deriv(valid_uuid, "")]);
        assert!(episode_derivations("t", "episode", &derivs).is_err());
    }

    #[test]
    fn non_episode_ignores_derivations() {
        assert!(episode_derivations("t", "observation", &None).is_ok());
        assert!(episode_derivations("t", "observation", &Some(vec![])).is_ok());
        assert!(episode_derivations("t", "decision", &None).is_ok());
    }
}
