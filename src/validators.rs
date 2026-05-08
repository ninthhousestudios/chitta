//! Domain contract validators — no I/O, no DB, independently unit-testable.
//!
//! These validate domain invariants (memory types, derivation rules, external
//! ref shapes, decision metadata). Cross-tool argument sanitisers (profile,
//! k, tags, etc.) live in [`crate::tools::validate`].

use serde_json::json;
use uuid::Uuid;

use crate::error::{ChittaError, Result};

const DECISION_ERROR_MSG: &str =
    "decision memory requires `metadata.rationale` and at least one \
     `metadata.rejected_alternatives` entry. Either supply them, demote \
     to memory_type=observation, or route to yojana.";

pub fn validate_decision_metadata(
    tool: &'static str,
    memory_type: &str,
    metadata: &Option<serde_json::Value>,
) -> Result<()> {
    if memory_type != "decision" {
        return Ok(());
    }

    let meta = match metadata {
        None => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "metadata".to_string(),
                constraint: DECISION_ERROR_MSG.to_string(),
                received: Some(json!(null)),
                next_action: DECISION_ERROR_MSG.to_string(),
            });
        }
        Some(v) => v,
    };

    let obj = meta.as_object().ok_or_else(|| ChittaError::InvalidArgument {
        tool,
        argument: "metadata".to_string(),
        constraint: DECISION_ERROR_MSG.to_string(),
        received: Some(meta.clone()),
        next_action: DECISION_ERROR_MSG.to_string(),
    })?;

    match obj.get("rationale").and_then(|v| v.as_str()) {
        None | Some("") => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "metadata.rationale".to_string(),
                constraint: DECISION_ERROR_MSG.to_string(),
                received: Some(json!(obj.get("rationale"))),
                next_action: DECISION_ERROR_MSG.to_string(),
            });
        }
        Some(_) => {}
    }

    match obj.get("rejected_alternatives").and_then(|v| v.as_array()) {
        None => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "metadata.rejected_alternatives".to_string(),
                constraint: DECISION_ERROR_MSG.to_string(),
                received: Some(json!(obj.get("rejected_alternatives"))),
                next_action: DECISION_ERROR_MSG.to_string(),
            });
        }
        Some(arr) if arr.is_empty() => {
            return Err(ChittaError::InvalidArgument {
                tool,
                argument: "metadata.rejected_alternatives".to_string(),
                constraint: DECISION_ERROR_MSG.to_string(),
                received: Some(json!([])),
                next_action: DECISION_ERROR_MSG.to_string(),
            });
        }
        Some(_) => {}
    }

    Ok(())
}

// ---- memory_type enum -----------------------------------------------

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

// ---- episode derivations --------------------------------------------

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

// ---- external refs --------------------------------------------------

pub const VALID_REF_KINDS: &[&str] = &["file", "commit", "yojana_task", "memory", "url", "session"];

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
fn parse_uuid(tool: &'static str, argument: &'static str, value: &str) -> Result<Uuid> {
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
    use serde_json::json;

    const T: &str = "store_memory";

    #[test]
    fn non_decision_type_passes() {
        assert!(validate_decision_metadata(T, "observation", &None).is_ok());
        assert!(validate_decision_metadata(T, "episode", &None).is_ok());
        assert!(validate_decision_metadata(T, "trait", &None).is_ok());
    }

    #[test]
    fn decision_missing_metadata_rejected() {
        let err = validate_decision_metadata(T, "decision", &None).unwrap_err();
        let data = err.data();
        assert_eq!(data.argument.as_deref(), Some("metadata"));
        assert!(data.constraint.contains("rationale"));
        assert!(data.next_action.contains("observation"));
    }

    #[test]
    fn decision_non_object_metadata_rejected() {
        let meta = Some(json!("just a string"));
        let err = validate_decision_metadata(T, "decision", &meta).unwrap_err();
        assert_eq!(err.data().argument.as_deref(), Some("metadata"));
    }

    #[test]
    fn decision_missing_rationale_rejected() {
        let meta = Some(json!({
            "rejected_alternatives": ["option B"]
        }));
        let err = validate_decision_metadata(T, "decision", &meta).unwrap_err();
        assert_eq!(err.data().argument.as_deref(), Some("metadata.rationale"));
    }

    #[test]
    fn decision_empty_rationale_rejected() {
        let meta = Some(json!({
            "rationale": "",
            "rejected_alternatives": ["option B"]
        }));
        let err = validate_decision_metadata(T, "decision", &meta).unwrap_err();
        assert_eq!(err.data().argument.as_deref(), Some("metadata.rationale"));
    }

    #[test]
    fn decision_missing_alternatives_rejected() {
        let meta = Some(json!({
            "rationale": "good reason"
        }));
        let err = validate_decision_metadata(T, "decision", &meta).unwrap_err();
        assert_eq!(
            err.data().argument.as_deref(),
            Some("metadata.rejected_alternatives")
        );
    }

    #[test]
    fn decision_empty_alternatives_rejected() {
        let meta = Some(json!({
            "rationale": "good reason",
            "rejected_alternatives": []
        }));
        let err = validate_decision_metadata(T, "decision", &meta).unwrap_err();
        assert_eq!(
            err.data().argument.as_deref(),
            Some("metadata.rejected_alternatives")
        );
    }

    #[test]
    fn decision_happy_path() {
        let meta = Some(json!({
            "rationale": "chose X because of Y",
            "rejected_alternatives": ["option A was too slow"]
        }));
        assert!(validate_decision_metadata(T, "decision", &meta).is_ok());
    }

    #[test]
    fn decision_multiple_alternatives_ok() {
        let meta = Some(json!({
            "rationale": "chose X",
            "rejected_alternatives": ["A", "B", "C"]
        }));
        assert!(validate_decision_metadata(T, "decision", &meta).is_ok());
    }

    // ---- memory_type ------------------------------------------------

    #[test]
    fn memory_type_rules() {
        for valid in VALID_MEMORY_TYPES {
            assert!(memory_type(T, valid).is_ok(), "{valid} should be valid");
        }
        assert!(memory_type(T, "bogus").is_err());
        assert!(memory_type(T, "Memory").is_err());
        assert!(memory_type(T, "").is_err());
    }

    #[test]
    fn memory_types_rules() {
        assert!(memory_types(T, &["observation".into(), "decision".into()]).is_ok());
        assert!(memory_types(T, &["observation".into(), "bogus".into()]).is_err());
        assert!(memory_types(T, &[]).is_ok());
    }

    // ---- external_refs ----------------------------------------------

    #[test]
    fn external_refs_valid() {
        let v = json!([{"kind": "file", "ref": "src/main.rs"}]);
        assert!(external_refs(T, &v).is_ok());

        let multi = json!([
            {"kind": "file", "ref": "a.rs"},
            {"kind": "commit", "ref": "abc123"},
            {"kind": "url", "ref": "https://example.com"},
        ]);
        assert!(external_refs(T, &multi).is_ok());

        assert!(external_refs(T, &json!([])).is_ok());
    }

    #[test]
    fn external_refs_not_array() {
        assert!(external_refs(T, &json!({"kind": "file", "ref": "x"})).is_err());
        assert!(external_refs(T, &json!("string")).is_err());
    }

    #[test]
    fn external_refs_bad_element() {
        assert!(external_refs(T, &json!(["not an object"])).is_err());
        assert!(external_refs(T, &json!([{"kind": "file"}])).is_err());
        assert!(external_refs(T, &json!([{"ref": "x"}])).is_err());
    }

    #[test]
    fn external_refs_invalid_kind() {
        assert!(external_refs(T, &json!([{"kind": "bogus", "ref": "x"}])).is_err());
    }

    #[test]
    fn external_refs_empty_ref() {
        assert!(external_refs(T, &json!([{"kind": "file", "ref": ""}])).is_err());
    }

    // ---- episode_derivations ----------------------------------------

    fn deriv(source_id: &str, dtype: &str) -> DerivationInput {
        DerivationInput {
            source_id: source_id.to_string(),
            derivation_type: dtype.to_string(),
        }
    }

    #[test]
    fn episode_derivations_rejects_none() {
        let r = episode_derivations(T, "episode", &None);
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("derivations"), "{msg}");
    }

    #[test]
    fn episode_derivations_rejects_empty() {
        let r = episode_derivations(T, "episode", &Some(vec![]));
        assert!(r.is_err());
    }

    #[test]
    fn episode_derivations_happy_path() {
        let valid_uuid = "019e0725-aab3-7160-905b-a150603d16d9";
        let derivs = Some(vec![deriv(valid_uuid, "synthesised_from")]);
        assert!(episode_derivations(T, "episode", &derivs).is_ok());
    }

    #[test]
    fn episode_derivations_multiple() {
        let u1 = "019e0725-aab3-7160-905b-a150603d16d9";
        let u2 = "019e0725-aab3-7160-905b-a150603d16da";
        let derivs = Some(vec![
            deriv(u1, "synthesised_from"),
            deriv(u2, "synthesised_from"),
        ]);
        assert!(episode_derivations(T, "episode", &derivs).is_ok());
    }

    #[test]
    fn episode_derivations_invalid_uuid() {
        let derivs = Some(vec![deriv("not-a-uuid", "synthesised_from")]);
        assert!(episode_derivations(T, "episode", &derivs).is_err());
    }

    #[test]
    fn episode_derivations_empty_type() {
        let valid_uuid = "019e0725-aab3-7160-905b-a150603d16d9";
        let derivs = Some(vec![deriv(valid_uuid, "")]);
        assert!(episode_derivations(T, "episode", &derivs).is_err());
    }

    #[test]
    fn non_episode_ignores_derivations() {
        assert!(episode_derivations(T, "observation", &None).is_ok());
        assert!(episode_derivations(T, "observation", &Some(vec![])).is_ok());
        assert!(episode_derivations(T, "decision", &None).is_ok());
    }
}
