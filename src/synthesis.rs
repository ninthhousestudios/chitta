use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, MemoryRow};
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::facets::Facets;

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

// ── clustering ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cluster {
    pub representative_claim: String,
    pub memory_type: String,
    pub source_ids: Vec<Uuid>,
}

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

#[derive(Debug, Deserialize)]
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
    let response = llm.complete(CLUSTER_SYSTEM_PROMPT, &user).await?;
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

// ── threshold ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ThresholdConfig {
    pub min_cluster_size: usize,
    pub min_distinct_days: usize,
    pub max_source_age_days: i64,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        Self {
            min_cluster_size: 5,
            min_distinct_days: 2,
            max_source_age_days: 90,
        }
    }
}

pub fn check_threshold(
    cluster: &Cluster,
    source_times: &HashMap<Uuid, DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &ThresholdConfig,
) -> bool {
    if cluster.source_ids.len() < config.min_cluster_size {
        return false;
    }

    let times: Vec<&DateTime<Utc>> = cluster
        .source_ids
        .iter()
        .filter_map(|id| source_times.get(id))
        .collect();

    let distinct_days: HashSet<chrono::NaiveDate> =
        times.iter().map(|t| t.date_naive()).collect();
    if distinct_days.len() < config.min_distinct_days {
        return false;
    }

    let cutoff = now - chrono::Duration::days(config.max_source_age_days);
    times.iter().any(|&&t| t >= cutoff)
}

// ── emission ────────────────────────────────────────────────────────

pub fn emission_confidence(cluster_size: usize) -> f32 {
    (0.50 + 0.05 * cluster_size as f32).min(0.90)
}

pub async fn emit_consolidated(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    cluster: &Cluster,
    profile: &str,
    now: DateTime<Utc>,
) -> Result<(MemoryRow, bool)> {
    let confidence = emission_confidence(cluster.source_ids.len());
    let id = Uuid::now_v7();

    let mut sorted_ids = cluster.source_ids.clone();
    sorted_ids.sort();
    let key_material = format!(
        "reflect:{}:{}",
        cluster.representative_claim,
        sorted_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let hash = Sha256::digest(key_material.as_bytes());
    let idem_key = format!(
        "reflect-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
        hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
    );

    let embed_out = embedder.embed_full(&cluster.representative_claim, "reflect").await?;
    let sparse_json = serde_json::to_value(&embed_out.sparse)
        .map_err(|e| ChittaError::Internal(format!("sparse serialization: {e}")))?;

    let row = MemoryRow {
        id,
        profile: profile.to_string(),
        content: cluster.representative_claim.clone(),
        embedding: Some(Vector::from(embed_out.dense)),
        sparse_embedding: Some(sparse_json),
        event_time: now,
        record_time: now,
        idempotency_key: idem_key,
        source: Some("reflect".into()),
        memory_type: cluster.memory_type.clone(),
        tags: vec!["reflect".into(), "synthesised".into()],
        external_refs: None,
        metadata: None,
        facets: Facets::default(),
        superseded_by: None,
        confidence: Some(confidence),
        reinforcement_count: 0,
        last_reinforced_at: None,
        invalidated_at: None,
    };

    let derivations: Vec<(Uuid, String)> = cluster
        .source_ids
        .iter()
        .map(|&sid| (sid, "synthesised_from".into()))
        .collect();

    db::insert_memory_with_derivations(pool, &row, &derivations).await
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

    // ── clustering tests ────────────────────────────────────────────

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
            Candidate { memory_type: "preference".into(), claim: "Josh prefers Rust".into(), source_id: ids[0] },
            Candidate { memory_type: "preference".into(), claim: "Josh likes Rust for systems".into(), source_id: ids[1] },
            Candidate { memory_type: "preference".into(), claim: "Josh chooses Rust".into(), source_id: ids[2] },
            Candidate { memory_type: "value".into(), claim: "Josh values simplicity".into(), source_id: ids[3] },
            Candidate { memory_type: "value".into(), claim: "Josh prefers simple designs".into(), source_id: ids[4] },
            Candidate { memory_type: "value".into(), claim: "Josh avoids complexity".into(), source_id: ids[5] },
        ];

        let llm = MockLlm::new(vec![r#"[
            {"representative_claim": "Josh prefers Rust for systems programming", "memory_type": "preference", "member_indices": [0, 1, 2]},
            {"representative_claim": "Josh values simplicity in design", "memory_type": "value", "member_indices": [3, 4, 5]}
        ]"#]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].representative_claim, "Josh prefers Rust for systems programming");
        assert_eq!(clusters[0].memory_type, "preference");
        assert_eq!(clusters[0].source_ids.len(), 3);
        assert_eq!(clusters[1].memory_type, "value");
    }

    #[tokio::test]
    async fn cluster_deduplicates_source_ids() {
        let id = Uuid::now_v7();
        let candidates = vec![
            Candidate { memory_type: "trait".into(), claim: "claim A".into(), source_id: id },
            Candidate { memory_type: "trait".into(), claim: "claim B".into(), source_id: id },
        ];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "merged claim", "memory_type": "trait", "member_indices": [0, 1]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].source_ids.len(), 1, "same source_id should be deduplicated");
    }

    #[tokio::test]
    async fn cluster_rejects_out_of_range_index() {
        let id = Uuid::now_v7();
        let candidates = vec![
            Candidate { memory_type: "trait".into(), claim: "a claim".into(), source_id: id },
        ];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "a claim", "memory_type": "trait", "member_indices": [0, 5]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert!(clusters.is_empty(), "cluster with out-of-range index should be skipped");
    }

    #[tokio::test]
    async fn cluster_rejects_duplicate_index_across_clusters() {
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        let candidates = vec![
            Candidate { memory_type: "trait".into(), claim: "claim A".into(), source_id: ids[0] },
            Candidate { memory_type: "trait".into(), claim: "claim B".into(), source_id: ids[1] },
            Candidate { memory_type: "value".into(), claim: "claim C".into(), source_id: ids[2] },
        ];

        let llm = MockLlm::new(vec![r#"[
            {"representative_claim": "cluster 1", "memory_type": "trait", "member_indices": [0, 1]},
            {"representative_claim": "cluster 2", "memory_type": "value", "member_indices": [1, 2]}
        ]"#]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert_eq!(clusters.len(), 1, "second cluster reusing index 1 should be skipped");
        assert_eq!(clusters[0].representative_claim, "cluster 1");
    }

    #[tokio::test]
    async fn cluster_filters_invalid_memory_type() {
        let id = Uuid::now_v7();
        let candidates = vec![
            Candidate { memory_type: "trait".into(), claim: "a claim".into(), source_id: id },
        ];

        let llm = MockLlm::new(vec![
            r#"[{"representative_claim": "a claim", "memory_type": "observation", "member_indices": [0]}]"#,
        ]);

        let clusters = cluster_candidates(&llm, &candidates).await.unwrap();
        assert!(clusters.is_empty(), "observation is not a valid consolidated type");
    }

    // ── threshold tests ─────────────────────────────────────────────

    fn make_source_times(ids: &[Uuid], times: &[DateTime<Utc>]) -> HashMap<Uuid, DateTime<Utc>> {
        ids.iter().copied().zip(times.iter().copied()).collect()
    }

    fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    fn make_cluster(source_ids: Vec<Uuid>) -> Cluster {
        Cluster {
            representative_claim: "test claim".into(),
            memory_type: "trait".into(),
            source_ids,
        }
    }

    #[test]
    fn threshold_below_size() {
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2026, 5, 1), utc(2026, 5, 2), utc(2026, 5, 3), utc(2026, 5, 4),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(!check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "4 sources should fail size>=5");
    }

    #[test]
    fn threshold_exactly_at_size() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2026, 5, 1), utc(2026, 5, 2), utc(2026, 5, 3),
            utc(2026, 5, 4), utc(2026, 5, 5),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "exactly 5 sources across 5 days with recent data should pass");
    }

    #[test]
    fn threshold_fails_distinct_days() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let same_day = utc(2026, 5, 10);
        let times = vec![same_day; 5];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(!check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "all sources on same day should fail distinct_days>=2");
    }

    #[test]
    fn threshold_fails_recency() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2025, 1, 1), utc(2025, 1, 2), utc(2025, 1, 3),
            utc(2025, 1, 4), utc(2025, 1, 5),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(!check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "all sources older than 90 days should fail recency check");
    }

    #[test]
    fn threshold_happy_path() {
        let ids: Vec<Uuid> = (0..7).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2025, 12, 1), utc(2025, 12, 15), utc(2026, 1, 10),
            utc(2026, 3, 5), utc(2026, 4, 20), utc(2026, 5, 1), utc(2026, 5, 10),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "7 sources across many days with recent entries should pass all checks");
    }

    // ── confidence tests ────────────────────────────────────────────

    #[test]
    fn confidence_at_min_cluster() {
        assert!((emission_confidence(5) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn confidence_scales_with_size() {
        assert!((emission_confidence(6) - 0.80).abs() < 1e-6);
        assert!((emission_confidence(7) - 0.85).abs() < 1e-6);
    }

    #[test]
    fn confidence_caps_at_090() {
        assert!((emission_confidence(8) - 0.90).abs() < 1e-6);
        assert!((emission_confidence(20) - 0.90).abs() < 1e-6);
        assert!((emission_confidence(100) - 0.90).abs() < 1e-6);
    }
}
