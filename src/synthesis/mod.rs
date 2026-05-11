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
pub use emission::{emit_consolidated, emit_with_supersession, emission_confidence};
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

        let active_existing: Vec<&MemoryRow> = existing
            .iter()
            .filter(|r| !superseded_ids.contains(&r.id))
            .collect();
        let active_refs: Vec<MemoryRow> = active_existing.into_iter().cloned().collect();

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
            emit_with_supersession(pool, embedder, cluster, &c, profile, now, source_facets)
                .await?;
            superseded_ids.insert(c.existing_id);
            result.supersessions += 1;
        } else {
            emit_consolidated(pool, embedder, cluster, profile, now, source_facets).await?;
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
}
