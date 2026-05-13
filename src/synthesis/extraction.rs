use uuid::Uuid;

use super::{strip_markdown_fences, Candidate, ExtractionStats, Llm, LLM_TIMEOUT, VALID_TYPES};
use crate::db::MemoryRow;
use crate::error::{ChittaError, Result};

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

#[derive(Debug, serde::Deserialize)]
struct RawCandidate {
    memory_type: String,
    claim: String,
}

pub async fn extract_candidates(
    llm: &(impl Llm + ?Sized),
    rows: &[MemoryRow],
) -> Result<ExtractionStats> {
    let mut stats = ExtractionStats {
        candidates: Vec::new(),
        rows_scanned: rows.len(),
        rows_skipped: 0,
        extraction_errors: 0,
    };
    let total = rows.len();
    for (i, row) in rows.iter().enumerate() {
        eprintln!("extraction: {}/{total}", i + 1);
        match extract_one(llm, row).await {
            Ok(candidates) => {
                if candidates.is_empty() {
                    stats.rows_skipped += 1;
                }
                stats.candidates.extend(candidates);
            }
            Err(e) => {
                stats.extraction_errors += 1;
                tracing::warn!(
                    memory_id = %row.id,
                    error = %e,
                    "skipping row: extraction failed"
                );
            }
        }
    }
    Ok(stats)
}

async fn extract_one(llm: &(impl Llm + ?Sized), row: &MemoryRow) -> Result<Vec<Candidate>> {
    let user = user_prompt(row);
    let response = tokio::time::timeout(LLM_TIMEOUT, llm.complete(SYSTEM_PROMPT, &user))
        .await
        .map_err(|_| ChittaError::Internal("LLM call timed out during extraction".into()))??;
    parse_response(&response, row.id)
}

fn parse_response(response: &str, source_id: Uuid) -> Result<Vec<Candidate>> {
    let trimmed = response.trim();
    let json_str = strip_markdown_fences(trimmed);

    let raw: Vec<RawCandidate> = serde_json::from_str(json_str)
        .map_err(|e| ChittaError::Internal(format!("LLM returned unparseable JSON: {e}")))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::test_support::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn zero_candidates() {
        let id = Uuid::now_v7();
        let row = make_row(id, "went for a walk", "observation");
        let llm = MockLlm::new(vec!["[]"]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert!(result.candidates.is_empty());
        assert_eq!(result.rows_scanned, 1);
        assert_eq!(result.rows_skipped, 1);
        assert_eq!(result.extraction_errors, 0);
    }

    #[tokio::test]
    async fn one_candidate() {
        let id = Uuid::now_v7();
        let row = make_row(id, "Josh always uses Vim for editing", "observation");
        let llm = MockLlm::new(vec![
            r#"[{"memory_type": "preference", "claim": "Josh prefers Vim as his primary editor"}]"#,
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].memory_type, "preference");
        assert_eq!(
            result.candidates[0].claim,
            "Josh prefers Vim as his primary editor"
        );
        assert_eq!(result.candidates[0].source_id, id);
    }

    #[tokio::test]
    async fn multiple_candidates_from_one_row() {
        let id = Uuid::now_v7();
        let row = make_row(
            id,
            "Josh values simplicity, prefers Rust, and always reviews PRs thoroughly",
            "observation",
        );
        let llm = MockLlm::new(vec![
            r#"[
            {"memory_type": "value", "claim": "Josh values simplicity in design"},
            {"memory_type": "preference", "claim": "Josh prefers Rust as his primary language"},
            {"memory_type": "pattern", "claim": "Josh reviews PRs thoroughly before merging"}
        ]"#,
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.candidates.len(), 3);
        assert_eq!(result.candidates[0].memory_type, "value");
        assert_eq!(result.candidates[1].memory_type, "preference");
        assert_eq!(result.candidates[2].memory_type, "pattern");
        assert!(result.candidates.iter().all(|c| c.source_id == id));
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
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].source_id, id2);
        assert_eq!(result.extraction_errors, 1);
    }

    #[tokio::test]
    async fn invalid_memory_type_filtered_out() {
        let id = Uuid::now_v7();
        let row = make_row(id, "some content", "observation");
        let llm = MockLlm::new(vec![
            r#"[
            {"memory_type": "preference", "claim": "valid claim"},
            {"memory_type": "observation", "claim": "wrong type, should be filtered"},
            {"memory_type": "trait", "claim": "another valid claim"}
        ]"#,
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].memory_type, "preference");
        assert_eq!(result.candidates[1].memory_type, "trait");
    }

    #[tokio::test]
    async fn markdown_fences_stripped() {
        let id = Uuid::now_v7();
        let row = make_row(id, "Josh likes TDD", "observation");
        let llm = MockLlm::new(vec![
            "```json\n[{\"memory_type\": \"preference\", \"claim\": \"Josh likes TDD\"}]\n```",
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.candidates.len(), 1);
    }

    #[tokio::test]
    async fn empty_claim_filtered_out() {
        let id = Uuid::now_v7();
        let row = make_row(id, "content", "observation");
        let llm = MockLlm::new(vec![
            r#"[
            {"memory_type": "trait", "claim": ""},
            {"memory_type": "value", "claim": "real claim"}
        ]"#,
        ]);

        let result = extract_candidates(&llm, &[row]).await.unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].claim, "real claim");
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
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].source_id, id1);
        assert_eq!(result.candidates[1].source_id, id2);
    }

    #[tokio::test]
    async fn llm_error_skipped_gracefully() {
        let id1 = Uuid::now_v7();
        let id2 = Uuid::now_v7();
        let rows = vec![
            make_row(id1, "first", "observation"),
            make_row(id2, "second", "observation"),
        ];
        let llm = MockLlm::new(vec![
            r#"[{"memory_type": "trait", "claim": "first claim"}]"#,
        ]);

        let result = extract_candidates(&llm, &rows).await.unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].source_id, id1);
        assert_eq!(result.extraction_errors, 1);
    }
}
