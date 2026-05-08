//! Pure validation functions — no I/O, no DB, independently unit-testable.

use serde_json::json;

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
}
