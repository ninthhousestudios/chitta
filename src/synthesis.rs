use std::future::Future;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::MemoryRow;
use crate::error::{ChittaError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub memory_type: String,
    pub claim: String,
    pub source_id: Uuid,
}

pub trait Llm: Send + Sync {
    fn complete(&self, system: &str, user: &str) -> impl Future<Output = Result<String>> + Send;
}

const SYSTEM_PROMPT: &str = "\
You extract consolidated claims about a person from raw memory entries. \
Each claim is a single, self-contained statement about a trait, value, \
pattern, preference, or mental model. Return a JSON array (possibly empty). \
Each element: {\"memory_type\": \"<trait|value|pattern|preference|mental_model>\", \"claim\": \"<statement>\"}. \
Return ONLY the JSON array, no markdown fences, no commentary.";

fn user_prompt(row: &MemoryRow) -> String {
    format!(
        "Memory type: {}\nTags: {}\nContent:\n{}",
        row.memory_type,
        row.tags.join(", "),
        row.content,
    )
}

#[derive(Debug, Deserialize)]
struct RawCandidate {
    memory_type: String,
    claim: String,
}

const VALID_TYPES: &[&str] = &["trait", "value", "pattern", "preference", "mental_model"];

pub async fn extract_candidates(
    llm: &(impl Llm + ?Sized),
    rows: &[MemoryRow],
) -> Result<Vec<Candidate>> {
    let mut all = Vec::new();
    for row in rows {
        match extract_one(llm, row).await {
            Ok(candidates) => all.extend(candidates),
            Err(e) => {
                tracing::warn!(
                    memory_id = %row.id,
                    error = %e,
                    "skipping row: extraction failed"
                );
            }
        }
    }
    Ok(all)
}

async fn extract_one(llm: &(impl Llm + ?Sized), row: &MemoryRow) -> Result<Vec<Candidate>> {
    let user = user_prompt(row);
    let response = llm.complete(SYSTEM_PROMPT, &user).await?;
    parse_response(&response, row.id)
}

fn parse_response(response: &str, source_id: Uuid) -> Result<Vec<Candidate>> {
    let trimmed = response.trim();
    let json_str = strip_markdown_fences(trimmed);

    let raw: Vec<RawCandidate> = serde_json::from_str(json_str).map_err(|e| {
        ChittaError::Internal(format!("LLM returned unparseable JSON: {e}"))
    })?;

    let candidates = raw
        .into_iter()
        .filter(|c| VALID_TYPES.contains(&c.memory_type.as_str()) && !c.claim.is_empty())
        .map(|c| Candidate {
            memory_type: c.memory_type,
            claim: c.claim,
            source_id,
        })
        .collect();

    Ok(candidates)
}

fn strip_markdown_fences(s: &str) -> &str {
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facets::Facets;
    use chrono::Utc;
    use std::sync::Mutex;

    struct MockLlm {
        responses: Mutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    impl Llm for MockLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(ChittaError::Internal("no more mock responses".into()))
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    fn make_row(id: Uuid, content: &str, memory_type: &str) -> MemoryRow {
        MemoryRow {
            id,
            profile: "josh".into(),
            content: content.into(),
            embedding: None,
            sparse_embedding: None,
            event_time: Utc::now(),
            record_time: Utc::now(),
            idempotency_key: format!("test-{id}"),
            source: None,
            memory_type: memory_type.into(),
            tags: vec![],
            external_refs: None,
            metadata: None,
            facets: Facets::default(),
            superseded_by: None,
            confidence: None,
            reinforcement_count: 0,
            last_reinforced_at: None,
            invalidated_at: None,
        }
    }

    #[tokio::test]
    async fn zero_candidates() {
        let id = Uuid::now_v7();
        let row = make_row(id, "went for a walk", "observation");
        let llm = MockLlm::new(vec!["[]"]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn one_candidate() {
        let id = Uuid::now_v7();
        let row = make_row(id, "Josh always uses Vim for editing", "observation");
        let llm = MockLlm::new(vec![
            r#"[{"memory_type": "preference", "claim": "Josh prefers Vim as his primary editor"}]"#,
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].memory_type, "preference");
        assert_eq!(result[0].claim, "Josh prefers Vim as his primary editor");
        assert_eq!(result[0].source_id, id);
    }

    #[tokio::test]
    async fn multiple_candidates_from_one_row() {
        let id = Uuid::now_v7();
        let row = make_row(
            id,
            "Josh values simplicity, prefers Rust, and always reviews PRs thoroughly",
            "observation",
        );
        let llm = MockLlm::new(vec![r#"[
            {"memory_type": "value", "claim": "Josh values simplicity in design"},
            {"memory_type": "preference", "claim": "Josh prefers Rust as his primary language"},
            {"memory_type": "pattern", "claim": "Josh reviews PRs thoroughly before merging"}
        ]"#]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].memory_type, "value");
        assert_eq!(result[1].memory_type, "preference");
        assert_eq!(result[2].memory_type, "pattern");
        assert!(result.iter().all(|c| c.source_id == id));
    }

    #[tokio::test]
    async fn malformed_response_skipped_gracefully() {
        let id1 = Uuid::now_v7();
        let id2 = Uuid::now_v7();
        let rows = vec![
            make_row(id1, "this row will get garbage", "observation"),
            make_row(id2, "this row will succeed", "observation"),
        ];
        let llm = MockLlm::new(vec![
            "not json at all {{{",
            r#"[{"memory_type": "trait", "claim": "Josh is pragmatic"}]"#,
        ]);

        let result = extract_candidates(&llm, &rows).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_id, id2);
    }

    #[tokio::test]
    async fn invalid_memory_type_filtered_out() {
        let id = Uuid::now_v7();
        let row = make_row(id, "some content", "observation");
        let llm = MockLlm::new(vec![r#"[
            {"memory_type": "preference", "claim": "valid claim"},
            {"memory_type": "observation", "claim": "wrong type, should be filtered"},
            {"memory_type": "trait", "claim": "another valid claim"}
        ]"#]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].memory_type, "preference");
        assert_eq!(result[1].memory_type, "trait");
    }

    #[tokio::test]
    async fn markdown_fences_stripped() {
        let id = Uuid::now_v7();
        let row = make_row(id, "Josh likes TDD", "observation");
        let llm = MockLlm::new(vec![
            "```json\n[{\"memory_type\": \"preference\", \"claim\": \"Josh likes TDD\"}]\n```",
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn empty_claim_filtered_out() {
        let id = Uuid::now_v7();
        let row = make_row(id, "content", "observation");
        let llm = MockLlm::new(vec![r#"[
            {"memory_type": "trait", "claim": ""},
            {"memory_type": "value", "claim": "real claim"}
        ]"#]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].claim, "real claim");
    }

    #[tokio::test]
    async fn multiple_rows_accumulate_candidates() {
        let id1 = Uuid::now_v7();
        let id2 = Uuid::now_v7();
        let rows = vec![
            make_row(id1, "first observation", "observation"),
            make_row(id2, "second observation", "observation"),
        ];
        let llm = MockLlm::new(vec![
            r#"[{"memory_type": "trait", "claim": "claim from row 1"}]"#,
            r#"[{"memory_type": "value", "claim": "claim from row 2"}]"#,
        ]);

        let result = extract_candidates(&llm, &rows).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].source_id, id1);
        assert_eq!(result[1].source_id, id2);
    }

    #[tokio::test]
    async fn llm_error_skipped_gracefully() {
        let id1 = Uuid::now_v7();
        let id2 = Uuid::now_v7();
        let rows = vec![
            make_row(id1, "first", "observation"),
            make_row(id2, "second", "observation"),
        ];
        // Only one response → second call will error
        let llm = MockLlm::new(vec![
            r#"[{"memory_type": "trait", "claim": "first claim"}]"#,
        ]);

        let result = extract_candidates(&llm, &rows).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_id, id1);
    }
}
