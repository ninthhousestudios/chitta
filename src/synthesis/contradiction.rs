use super::{strip_markdown_fences, Contradiction, Llm, LLM_TIMEOUT};
use crate::db::MemoryRow;
use crate::error::{ChittaError, Result};

const CONTRADICTION_SYSTEM_PROMPT: &str = "\
You detect contradictions between a new claim about a person and their existing \
consolidated beliefs/traits/values/patterns/preferences. A contradiction means \
the new claim directly opposes or invalidates an existing one — not merely \
refines, adds nuance, or covers different ground. \
If a contradiction exists, return: {\"contradicts_index\": <index>, \"shift\": \"<description>\"}. \
If no contradiction, return: {\"contradicts_index\": null}. \
Return ONLY the JSON object, no markdown fences, no commentary.";

fn contradiction_user_prompt(new_claim: &str, existing: &[MemoryRow]) -> String {
    let mut lines = Vec::with_capacity(existing.len() + 3);
    lines.push("Existing consolidated memories:".to_string());
    for (i, row) in existing.iter().enumerate() {
        lines.push(format!("[{i}] ({}) {}", row.memory_type, row.content));
    }
    lines.push(String::new());
    lines.push(format!("New claim: {new_claim}"));
    lines.join("\n")
}

#[derive(Debug, serde::Deserialize)]
struct RawContradiction {
    contradicts_index: Option<usize>,
    #[serde(default)]
    shift: Option<String>,
}

pub async fn detect_contradiction(
    llm: &(impl Llm + ?Sized),
    cluster_claim: &str,
    existing: &[MemoryRow],
) -> Result<Option<Contradiction>> {
    if existing.is_empty() {
        return Ok(None);
    }

    let user = contradiction_user_prompt(cluster_claim, existing);
    let response = tokio::time::timeout(
        LLM_TIMEOUT,
        llm.complete(CONTRADICTION_SYSTEM_PROMPT, &user),
    )
    .await
    .map_err(|_| {
        ChittaError::Internal("LLM call timed out during contradiction detection".into())
    })??;
    parse_contradiction_response(&response, existing)
}

fn parse_contradiction_response(
    response: &str,
    existing: &[MemoryRow],
) -> Result<Option<Contradiction>> {
    let json_str = strip_markdown_fences(response.trim());
    let raw: RawContradiction = serde_json::from_str(json_str).map_err(|e| {
        ChittaError::Internal(format!("LLM returned unparseable contradiction JSON: {e}"))
    })?;

    match raw.contradicts_index {
        None => Ok(None),
        Some(idx) if idx >= existing.len() => {
            tracing::warn!(
                index = idx,
                max = existing.len(),
                "contradiction index out of range"
            );
            Ok(None)
        }
        Some(idx) => {
            let shift = raw.shift.unwrap_or_else(|| {
                format!("Changed from '{}' to a new position", existing[idx].content)
            });
            Ok(Some(Contradiction {
                existing_id: existing[idx].id,
                existing_claim: existing[idx].content.clone(),
                shift_description: shift,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::test_support::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn contradiction_detected() {
        let existing_id = Uuid::now_v7();
        let existing = vec![make_consolidated_row(
            existing_id,
            "Josh prefers tabs over spaces",
            "preference",
        )];
        let llm = MockLlm::new(vec![
            r#"{"contradicts_index": 0, "shift": "switched from tabs to spaces"}"#,
        ]);

        let result = detect_contradiction(&llm, "Josh prefers spaces over tabs", &existing)
            .await
            .unwrap();
        let c = result.expect("should detect contradiction");
        assert_eq!(c.existing_id, existing_id);
        assert_eq!(c.existing_claim, "Josh prefers tabs over spaces");
        assert_eq!(c.shift_description, "switched from tabs to spaces");
    }

    #[tokio::test]
    async fn no_contradiction() {
        let existing = vec![make_consolidated_row(
            Uuid::now_v7(),
            "Josh prefers Rust",
            "preference",
        )];
        let llm = MockLlm::new(vec![r#"{"contradicts_index": null}"#]);

        let result = detect_contradiction(&llm, "Josh values simplicity", &existing)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn contradiction_empty_existing() {
        let llm = MockLlm::new(vec![]);
        let result = detect_contradiction(&llm, "any claim", &[]).await.unwrap();
        assert!(result.is_none(), "no existing rows → no contradiction");
    }

    #[tokio::test]
    async fn contradiction_out_of_range_index() {
        let existing = vec![make_consolidated_row(
            Uuid::now_v7(),
            "Josh prefers Rust",
            "preference",
        )];
        let llm = MockLlm::new(vec![r#"{"contradicts_index": 5, "shift": "bogus"}"#]);

        let result = detect_contradiction(&llm, "some claim", &existing)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "out-of-range index should be treated as no contradiction"
        );
    }

    #[tokio::test]
    async fn contradiction_multiple_existing_picks_correct() {
        let id0 = Uuid::now_v7();
        let id1 = Uuid::now_v7();
        let id2 = Uuid::now_v7();
        let existing = vec![
            make_consolidated_row(id0, "Josh prefers Vim", "preference"),
            make_consolidated_row(id1, "Josh dislikes meetings", "preference"),
            make_consolidated_row(id2, "Josh values simplicity", "value"),
        ];
        let llm = MockLlm::new(vec![
            r#"{"contradicts_index": 1, "shift": "now enjoys collaborative meetings"}"#,
        ]);

        let result = detect_contradiction(&llm, "Josh finds meetings productive", &existing)
            .await
            .unwrap();
        let c = result.expect("should detect contradiction");
        assert_eq!(c.existing_id, id1);
        assert_eq!(c.existing_claim, "Josh dislikes meetings");
    }

    #[tokio::test]
    async fn contradiction_missing_shift_gets_default() {
        let id = Uuid::now_v7();
        let existing = vec![make_consolidated_row(id, "Josh prefers tabs", "preference")];
        let llm = MockLlm::new(vec![r#"{"contradicts_index": 0}"#]);

        let result = detect_contradiction(&llm, "Josh prefers spaces", &existing)
            .await
            .unwrap();
        let c = result.unwrap();
        assert!(c.shift_description.contains("Josh prefers tabs"));
    }
}
