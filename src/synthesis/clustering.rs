use std::collections::HashSet;

use uuid::Uuid;

use super::{strip_markdown_fences, Candidate, Cluster, Llm, LLM_TIMEOUT, VALID_TYPES};
use crate::error::{ChittaError, Result};

const CLUSTER_SYSTEM_PROMPT: &str = "\
You group candidate claims about a person by semantic similarity. \
Claims expressing the same underlying trait, value, pattern, preference, \
or mental model — even if worded differently — belong in the same cluster. \
For each cluster, pick the single best representative claim and the most \
appropriate memory_type. Return a JSON array. Each element: \
{\"representative_claim\": \"<best wording>\", \"memory_type\": \"<trait|value|pattern|preference|mental_model>\", \
\"member_indices\": [0, 1, ...]}. \
Every candidate index must appear in exactly one cluster. \
Return ONLY the JSON array, no markdown fences, no commentary.";

fn cluster_user_prompt(candidates: &[Candidate]) -> String {
    let mut lines = Vec::with_capacity(candidates.len() + 1);
    lines.push("Candidates:".to_string());
    for (i, c) in candidates.iter().enumerate() {
        lines.push(format!("[{i}] ({}) {}", c.memory_type, c.claim));
    }
    lines.join("\n")
}

#[derive(Debug, serde::Deserialize)]
struct RawCluster {
    representative_claim: String,
    memory_type: String,
    member_indices: Vec<usize>,
}

pub async fn cluster_candidates(
    llm: &(impl Llm + ?Sized),
    candidates: &[Candidate],
) -> Result<Vec<Cluster>> {
    if candidates.is_empty() {
        return Ok(vec![]);
    }

    let user = cluster_user_prompt(candidates);
    let response = tokio::time::timeout(LLM_TIMEOUT, llm.complete(CLUSTER_SYSTEM_PROMPT, &user))
        .await
        .map_err(|_| ChittaError::Internal("LLM call timed out during clustering".into()))??;
    parse_cluster_response(&response, candidates)
}

fn parse_cluster_response(response: &str, candidates: &[Candidate]) -> Result<Vec<Cluster>> {
    let json_str = strip_markdown_fences(response.trim());

    let raw: Vec<RawCluster> = serde_json::from_str(json_str).map_err(|e| {
        ChittaError::Internal(format!("LLM returned unparseable cluster JSON: {e}"))
    })?;

    let mut seen_indices: HashSet<usize> = HashSet::new();

    let clusters = raw
        .into_iter()
        .filter_map(|rc| {
            if !VALID_TYPES.contains(&rc.memory_type.as_str()) {
                tracing::warn!(memory_type = %rc.memory_type, "cluster has invalid memory_type, skipping");
                return None;
            }
            if rc.representative_claim.is_empty() {
                return None;
            }

            for &i in &rc.member_indices {
                if i >= candidates.len() {
                    tracing::warn!(index = i, max = candidates.len(), "cluster index out of range, skipping cluster");
                    return None;
                }
                if !seen_indices.insert(i) {
                    tracing::warn!(index = i, "duplicate cluster index, skipping cluster");
                    return None;
                }
            }

            let source_ids: Vec<Uuid> = rc
                .member_indices
                .iter()
                .map(|&i| candidates[i].source_id)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            if source_ids.is_empty() {
                return None;
            }

            Some(Cluster {
                representative_claim: rc.representative_claim,
                memory_type: rc.memory_type,
                source_ids,
            })
        })
        .collect();

    Ok(clusters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::test_support::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn cluster_empty_candidates() {
        let llm = MockLlm::new(vec![]);
        let result = cluster_candidates(&llm, &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn cluster_groups_similar_claims() {
        let ids: Vec<Uuid> = (0..6).map(|_| Uuid::now_v7()).collect();
        let candidates = vec![
            Candidate {
                memory_type: "preference".into(),
                claim: "Josh prefers Rust".into(),
                source_id: ids[0],
            },
            Candidate {
                memory_type: "preference".into(),
                claim: "Josh likes Rust for systems".into(),
                source_id: ids[1],
            },
            Candidate {
                memory_type: "preference".into(),
                claim: "Josh chooses Rust".into(),
                source_id: ids[2],
            },
            Candidate {
                memory_type: "value".into(),
                claim: "Josh values simplicity".into(),
                source_id: ids[3],
            },
            Candidate {
                memory_type: "value".into(),
                claim: "Josh prefers simple designs".into(),
                source_id: ids[4],
            },
            Candidate {
                memory_type: "value".into(),
                claim: "Josh avoids complexity".into(),
                source_id: ids[5],
            },
        ];

        let llm = MockLlm::new(vec![
            r#"[
            {"representative_claim": "Josh prefers Rust for systems programming", "memory_type": "preference", "member_indices": [0, 1, 2]},
            {"representative_claim": "Josh values simplicity in design", "memory_type": "value", "member_indices": [3, 4, 5]}
        ]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(
            clusters[0].representative_claim,
            "Josh prefers Rust for systems programming"
        );
        assert_eq!(clusters[0].memory_type, "preference");
        assert_eq!(clusters[0].source_ids.len(), 3);
        assert_eq!(clusters[1].memory_type, "value");
    }

    #[tokio::test]
    async fn cluster_deduplicates_source_ids() {
        let id = Uuid::now_v7();
        let candidates = vec![
            Candidate {
                memory_type: "trait".into(),
                claim: "claim A".into(),
                source_id: id,
            },
            Candidate {
                memory_type: "trait".into(),
                claim: "claim B".into(),
                source_id: id,
            },
        ];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "merged claim", "memory_type": "trait", "member_indices": [0, 1]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].source_ids.len(),
            1,
            "same source_id should be deduplicated"
        );
    }

    #[tokio::test]
    async fn cluster_rejects_out_of_range_index() {
        let id = Uuid::now_v7();
        let candidates = vec![Candidate {
            memory_type: "trait".into(),
            claim: "a claim".into(),
            source_id: id,
        }];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "a claim", "memory_type": "trait", "member_indices": [0, 5]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert!(
            clusters.is_empty(),
            "cluster with out-of-range index should be skipped"
        );
    }

    #[tokio::test]
    async fn cluster_rejects_duplicate_index_across_clusters() {
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        let candidates = vec![
            Candidate {
                memory_type: "trait".into(),
                claim: "claim A".into(),
                source_id: ids[0],
            },
            Candidate {
                memory_type: "trait".into(),
                claim: "claim B".into(),
                source_id: ids[1],
            },
            Candidate {
                memory_type: "value".into(),
                claim: "claim C".into(),
                source_id: ids[2],
            },
        ];

        let llm = MockLlm::new(vec![
            r#"[
            {"representative_claim": "cluster 1", "memory_type": "trait", "member_indices": [0, 1]},
            {"representative_claim": "cluster 2", "memory_type": "value", "member_indices": [1, 2]}
        ]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(
            clusters.len(),
            1,
            "second cluster reusing index 1 should be skipped"
        );
        assert_eq!(clusters[0].representative_claim, "cluster 1");
    }

    #[tokio::test]
    async fn cluster_filters_invalid_memory_type() {
        let id = Uuid::now_v7();
        let candidates = vec![Candidate {
            memory_type: "trait".into(),
            claim: "a claim".into(),
            source_id: id,
        }];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "a claim", "memory_type": "observation", "member_indices": [0]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert!(
            clusters.is_empty(),
            "observation is not a valid consolidated type"
        );
    }
}
