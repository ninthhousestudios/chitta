//! Hybrid retrieval via Reciprocal Rank Fusion (RRF).

use std::collections::HashMap;

use sqlx::PgPool;
use uuid::Uuid;

use crate::config::SearchConfig;
use crate::db::{self, SearchHit};
use crate::embedding::EmbedOutput;
use crate::error::Result;
use crate::facets::Facets;
use pgvector::Vector;

pub struct HybridSearchParams<'a> {
    pub profile: &'a str,
    pub query_embed: &'a EmbedOutput,
    pub k: i64,
    pub tags: &'a [String],
    pub memory_types: &'a [String],
    pub min_similarity: f32,
    pub recency_weight: f32,
    pub recency_half_life_days: f32,
    pub search_cfg: &'a SearchConfig,
    pub query_text: &'a str,
    pub exclude_invalidated: bool,
    pub exclude_superseded: bool,
    pub ref_filter_json: Option<&'a serde_json::Value>,
    pub facets: &'a Facets,
}

#[tracing::instrument(
    name = "retrieval.search_hybrid",
    skip(pool, p),
    fields(k = p.k, profile = p.profile)
)]
pub async fn search_hybrid(
    pool: &PgPool,
    p: &HybridSearchParams<'_>,
) -> Result<(Vec<SearchHit>, i64)> {
    let fetch_limit = p.k * p.search_cfg.rrf_candidates;
    let query_vec = Vector::from(p.query_embed.dense.clone());

    let dense_params = db::SearchParams {
        profile: p.profile,
        query: &query_vec,
        k: fetch_limit,
        tags: p.tags,
        memory_types: p.memory_types,
        min_similarity: 0.0,
        recency_weight: 0.0,
        recency_half_life_days: p.recency_half_life_days,
        exclude_invalidated: p.exclude_invalidated,
        exclude_superseded: p.exclude_superseded,
        ref_filter_json: p.ref_filter_json,
        facets: p.facets,
    };
    let dense_fut = db::search_by_embedding(pool, &dense_params);

    let (dense_hits, total) = dense_fut.await?;
    let fts_ids: Vec<Uuid> = vec![];

    // Build rank lists: doc -> rank (0-based).
    let mut rrf_scores: HashMap<Uuid, f32> = HashMap::new();
    let k_const = p.search_cfg.rrf_k as f32;

    for (rank, hit) in dense_hits.iter().enumerate() {
        *rrf_scores.entry(hit.id).or_default() += 1.0 / (k_const + rank as f32);
    }
    for (rank, id) in fts_ids.iter().enumerate() {
        *rrf_scores.entry(*id).or_default() += 1.0 / (k_const + rank as f32);
    }

    // Sparse re-ranking leg: operates on the candidate pool from dense + FTS.
    if p.search_cfg.rrf_sparse && !p.query_embed.sparse.is_empty() {
        let candidate_ids: Vec<Uuid> = rrf_scores.keys().copied().collect();
        if !candidate_ids.is_empty() {
            match db::fetch_sparse_embeddings(pool, &candidate_ids).await {
                Ok(sparse_docs) => {
                    let mut sparse_scored: Vec<(Uuid, f32)> = sparse_docs
                        .into_iter()
                        .map(|(id, doc_sparse)| {
                            let dot: f32 = p
                                .query_embed
                                .sparse
                                .iter()
                                .filter_map(|(tok, qw)| doc_sparse.get(tok).map(|dw| qw * dw))
                                .sum();
                            (id, dot)
                        })
                        .collect();
                    sparse_scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    for (rank, (id, _score)) in sparse_scored.iter().enumerate() {
                        *rrf_scores.entry(*id).or_default() += 1.0 / (k_const + rank as f32);
                    }
                }
                Err(e) => {
                    tracing::warn!("Sparse leg failed, continuing without: {e}");
                }
            }
        }
    }

    // Sort by RRF score descending, take top k.
    let mut ranked: Vec<(Uuid, f32)> = rrf_scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(p.k as usize);

    if ranked.is_empty() {
        return Ok((vec![], total));
    }

    let final_ids: Vec<Uuid> = ranked.iter().map(|(id, _)| *id).collect();
    let score_map: HashMap<Uuid, f32> = ranked.into_iter().collect();

    // Preserve raw cosine similarity from the dense leg for each candidate.
    let dense_sim: HashMap<Uuid, f32> = dense_hits.iter().map(|h| (h.id, h.similarity)).collect();

    // Hydrate full SearchHit results and stamp scores.
    let mut hits = db::fetch_search_hits_by_ids(pool, p.profile, &final_ids).await?;
    for hit in &mut hits {
        hit.similarity = dense_sim.get(&hit.id).copied().unwrap_or(0.0);
        hit.score = score_map.get(&hit.id).copied().unwrap_or(0.0);
    }

    // Apply recency re-ranking post-fusion if enabled.
    if p.recency_weight > 0.0 {
        let now = chrono::Utc::now();
        let hl_secs = (p.recency_half_life_days as f64) * 86400.0;
        for hit in &mut hits {
            let age_secs = (now - hit.event_time).num_seconds().max(0) as f64;
            let recency_factor = (-age_secs / hl_secs).exp() as f32;
            hit.score *= 1.0 + p.recency_weight * recency_factor;
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    apply_type_weights(&mut hits, &p.search_cfg.type_weights);

    if p.min_similarity > 0.0 {
        hits.retain(|h| h.similarity >= p.min_similarity);
    }

    Ok((hits, total))
}

pub fn apply_type_weights(hits: &mut [db::SearchHit], weights: &HashMap<String, f32>) {
    if weights.is_empty() {
        return;
    }
    for hit in hits.iter_mut() {
        if let Some(&w) = weights.get(&hit.memory_type) {
            hit.score *= w;
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    fn rrf_score(k: u32, ranks: &[usize]) -> f32 {
        ranks.iter().map(|&r| 1.0 / (k as f32 + r as f32)).sum()
    }

    #[test]
    fn rrf_formula_basic() {
        // Document at rank 0 in two legs with k=60.
        let score = rrf_score(60, &[0, 0]);
        let expected = 2.0 / 60.0;
        assert!((score - expected).abs() < 1e-6);
    }

    #[test]
    fn rrf_formula_different_ranks() {
        // Rank 0 in dense, rank 5 in FTS, k=60.
        let score = rrf_score(60, &[0, 5]);
        let expected = 1.0 / 60.0 + 1.0 / 65.0;
        assert!((score - expected).abs() < 1e-6);
    }

    #[test]
    fn rrf_single_leg_identity() {
        // Document in only one leg should still get a score.
        let score = rrf_score(60, &[3]);
        let expected = 1.0 / 63.0;
        assert!((score - expected).abs() < 1e-6);
    }

    #[test]
    fn rrf_higher_rank_beats_lower() {
        // Doc at rank 0 in one leg should outscore doc at rank 10 in one leg.
        let score_top = rrf_score(60, &[0]);
        let score_bottom = rrf_score(60, &[10]);
        assert!(score_top > score_bottom);
    }

    #[test]
    fn rrf_two_legs_beats_one() {
        // Doc in two legs (both at rank 5) should outscore doc in one leg at rank 0.
        let two_legs = rrf_score(60, &[5, 5]);
        let one_leg = rrf_score(60, &[0]);
        assert!(two_legs > one_leg);
    }
}
