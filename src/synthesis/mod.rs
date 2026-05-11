use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, MemoryRow};
use crate::embedding::Embedder;
use crate::error::Result;
use crate::facets::Facets;

mod clustering;
mod contradiction;
mod disagree;
mod emission;
mod extraction;
mod threshold;

pub use clustering::cluster_candidates;
pub use contradiction::detect_contradiction;
pub use disagree::find_disagree_targets;
pub(crate) use emission::{emit_consolidated, emit_with_supersession};
pub use emission::{emit_consolidated_auto, emission_confidence};
pub use extraction::extract_candidates;
pub use threshold::check_threshold;

pub(crate) const LLM_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) const VALID_TYPES: &[&str] = &["trait", "value", "pattern", "preference", "mental_model"];

pub(crate) fn strip_markdown_fences(s: &str) -> &str {
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub memory_type: String,
    pub claim: String,
    pub source_id: Uuid,
}

pub trait Llm: Send + Sync {
    fn complete(&self, system: &str, user: &str) -> impl Future<Output = Result<String>> + Send;
}

pub struct ExtractionStats {
    pub candidates: Vec<Candidate>,
    pub rows_scanned: usize,
    pub rows_skipped: usize,
    pub extraction_errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cluster {
    pub representative_claim: String,
    pub memory_type: String,
    pub source_ids: Vec<Uuid>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Contradiction {
    pub existing_id: Uuid,
    pub existing_claim: String,
    pub shift_description: String,
}

pub struct SupersessionResult {
    pub new_row: MemoryRow,
    pub superseded_id: Uuid,
    pub meta_row_id: Uuid,
    pub idempotent_replay: bool,
}

const PRE_FILTER_TOP_K: usize = 10;

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn pre_filter_existing<'a>(
    claim_embedding: &[f32],
    existing: &'a [MemoryRow],
    top_k: usize,
) -> Vec<&'a MemoryRow> {
    let mut embedded: Vec<(f32, &MemoryRow)> = Vec::new();
    let mut unembedded: Vec<&MemoryRow> = Vec::new();

    for row in existing {
        match row.embedding.as_ref() {
            Some(emb) => {
                let sim = cosine_similarity(claim_embedding, emb.as_slice());
                embedded.push((sim, row));
            }
            None => unembedded.push(row),
        }
    }

    embedded.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut result: Vec<&MemoryRow> =
        embedded.into_iter().take(top_k).map(|(_, row)| row).collect();
    result.extend(unembedded);
    result
}

pub struct SynthesisResult {
    pub clusters_formed: usize,
    pub clusters_emitted: usize,
    pub supersessions: usize,
    pub rows_scanned: usize,
    pub rows_skipped: usize,
    pub extraction_errors: usize,
}

pub async fn run_synthesis(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    llm: &(impl Llm + ?Sized),
    profile: &str,
    rows: &[MemoryRow],
    now: DateTime<Utc>,
) -> Result<SynthesisResult> {
    let extraction = extract_candidates(llm, rows).await?;
    let clusters = cluster_candidates(llm, &extraction.candidates).await?;

    let source_times: HashMap<Uuid, DateTime<Utc>> =
        rows.iter().map(|r| (r.id, r.record_time)).collect();
    let rows_by_id: HashMap<Uuid, &MemoryRow> = rows.iter().map(|r| (r.id, r)).collect();
    let config = ThresholdConfig::default();

    let mut existing = db::fetch_profile_candidates(pool, profile).await?;

    let disagree_targets: HashSet<Uuid> = find_disagree_targets(rows).into_iter().collect();
    let existing_ids: HashSet<Uuid> = existing.iter().map(|r| r.id).collect();
    for &target_id in &disagree_targets {
        if !existing_ids.contains(&target_id) {
            if let Some(row) = db::get_memory_by_id(pool, profile, target_id).await? {
                if row.superseded_by.is_none() && row.invalidated_at.is_none() {
                    existing.push(row);
                }
            }
        }
    }

    let mut superseded_ids: HashSet<Uuid> = HashSet::new();

    let mut result = SynthesisResult {
        clusters_formed: clusters.len(),
        clusters_emitted: 0,
        supersessions: 0,
        rows_scanned: extraction.rows_scanned,
        rows_skipped: extraction.rows_skipped,
        extraction_errors: extraction.extraction_errors,
    };

    for cluster in &clusters {
        if !check_threshold(cluster, &source_times, now, &config) {
            continue;
        }

        let claim_embedding = embedder
            .embed_full(&cluster.representative_claim, "reflect")
            .await?;

        let active_existing: Vec<MemoryRow> = existing
            .iter()
            .filter(|r| !superseded_ids.contains(&r.id))
            .cloned()
            .collect();
        let active_refs: Vec<MemoryRow> =
            pre_filter_existing(&claim_embedding.dense, &active_existing, PRE_FILTER_TOP_K)
                .into_iter()
                .cloned()
                .collect();

        let contradiction =
            detect_contradiction(llm, &cluster.representative_claim, &active_refs).await?;

        let source_facet_list: Vec<Facets> = cluster
            .source_ids
            .iter()
            .filter_map(|id| rows_by_id.get(id))
            .map(|r| r.facets.clone())
            .collect();
        let source_facets = Facets::distinct_union(&source_facet_list);

        if let Some(c) = contradiction {
            emit_with_supersession(
                pool, embedder, &claim_embedding, cluster, &c, profile, now, source_facets,
            )
            .await?;
            superseded_ids.insert(c.existing_id);
            result.supersessions += 1;
        } else {
            emit_consolidated(pool, &claim_embedding, cluster, profile, now, source_facets)
                .await?;
        }
        result.clusters_emitted += 1;
    }

    Ok(result)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::facets::Facets;

    pub struct MockLlm {
        responses: Mutex<Vec<String>>,
    }

    impl MockLlm {
        pub fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    impl Llm for MockLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(crate::error::ChittaError::Internal(
                    "no more mock responses".into(),
                ))
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    pub fn make_row(id: Uuid, content: &str, memory_type: &str) -> MemoryRow {
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

    pub fn make_consolidated_row(id: Uuid, content: &str, memory_type: &str) -> MemoryRow {
        MemoryRow {
            id,
            profile: "josh".into(),
            content: content.into(),
            embedding: None,
            sparse_embedding: None,
            event_time: Utc::now(),
            record_time: Utc::now(),
            idempotency_key: format!("consolidated-{id}"),
            source: Some("reflect".into()),
            memory_type: memory_type.into(),
            tags: vec!["reflect".into(), "synthesised".into()],
            external_refs: None,
            metadata: None,
            facets: Facets::default(),
            superseded_by: None,
            confidence: Some(0.75),
            reinforcement_count: 0,
            last_reinforced_at: None,
            invalidated_at: None,
        }
    }

    pub fn make_consolidated_row_with_embedding(
        id: Uuid,
        content: &str,
        memory_type: &str,
        embedding: Vec<f32>,
    ) -> MemoryRow {
        let mut row = make_consolidated_row(id, content, memory_type);
        row.embedding = Some(pgvector::Vector::from(embedding));
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::*;
    use uuid::Uuid;

    #[test]
    fn cosine_similarity_identical_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![0.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn pre_filter_returns_top_k_most_similar() {
        let claim = vec![1.0, 0.0, 0.0];

        let rows = vec![
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "orthogonal", "trait", vec![0.0, 1.0, 0.0],
            ),
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "very similar", "trait", vec![0.9, 0.1, 0.0],
            ),
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "somewhat similar", "trait", vec![0.5, 0.5, 0.0],
            ),
        ];

        let filtered = pre_filter_existing(&claim, &rows, 2);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].content, "very similar");
        assert_eq!(filtered[1].content, "somewhat similar");
    }

    #[test]
    fn pre_filter_includes_unembedded_rows_as_fallback() {
        let claim = vec![1.0, 0.0, 0.0];

        let rows = vec![
            make_consolidated_row(Uuid::now_v7(), "no embedding", "trait"),
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "has embedding", "trait", vec![1.0, 0.0, 0.0],
            ),
        ];

        let filtered = pre_filter_existing(&claim, &rows, 10);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].content, "has embedding");
        assert_eq!(filtered[1].content, "no embedding");
    }

    #[test]
    fn pre_filter_returns_all_when_fewer_than_k() {
        let claim = vec![1.0, 0.0, 0.0];

        let rows = vec![
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "a", "trait", vec![1.0, 0.0, 0.0],
            ),
            make_consolidated_row_with_embedding(
                Uuid::now_v7(), "b", "trait", vec![0.5, 0.5, 0.0],
            ),
        ];

        let filtered = pre_filter_existing(&claim, &rows, 10);
        assert_eq!(filtered.len(), 2);
    }
}
