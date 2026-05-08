//! `search_memories` handler.
//!
//! Semantic similarity with tag-OR filter and min-similarity floor.
//! Returns the standard envelope; results carry 200-char verbatim snippets
//! by default. Pass `include_content: true` to get full content + metadata
//! per hit (avoids follow-up `get_memory` calls).

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::SearchConfig;
use crate::db;
use crate::embedding::Embedder;
use crate::envelope::{Envelope, estimate_tokens};
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "search_memories";

/// Default `k` when the caller does not set one.
const DEFAULT_K: i64 = 10;

/// Snippet length in chars (not bytes). Verbatim prefix; no ellipsis.
const SNIPPET_CHARS: usize = 200;

/// Filter by external ref. Only returns memories whose `external_refs`
/// contain a matching entry.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RefFilter {
    /// Ref kind. Valid kinds: file, commit, yojana_task, memory, url, session.
    pub kind: String,
    /// Ref value. When provided, only matches refs with this exact value.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_value: Option<String>,
}

/// Context-faceted filter. Each supplied facet intersects with the row's
/// corresponding `applies_to_*` column via array containment (`@>`).
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct AppliesTo {
    /// Filter by domain facet (e.g. "rust", "astrology").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains: Option<Vec<String>>,
    /// Filter by skill facet (e.g. "review", "reflect").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Filter by project facet (e.g. "chitta", "sutra").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<String>>,
    /// Filter by situation facet (e.g. "debugging", "planning").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub situations: Option<Vec<String>>,
}

const CONSOLIDATED_TYPES: &[&str] = &["trait", "value", "pattern", "preference", "mental_model"];

/// Arguments for `search_memories`. `JsonSchema` is derived so rmcp exposes
/// the same shape callers use on the wire.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SearchArgs {
    /// Profile scope.
    pub profile: String,
    /// Natural-language query.
    pub query: String,
    /// Max number of results. Default 10; hard cap `validate::MAX_K`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<i64>,
    /// Stop adding results once `budget_spent_tokens` would exceed this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// OR-match: a memory matches if it has any of these tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Cosine-similarity floor in `[0.0, 1.0]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_similarity: Option<f32>,
    /// Return full `content` and `metadata` per hit instead of snippet only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_content: Option<bool>,
    /// Filter by memory type(s). OR-match: returns memories matching any listed type.
    /// Valid types: observation, episode, decision, trait, value, pattern, preference, mental_model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_types: Option<Vec<String>>,
    /// Exclude soft-deleted memories. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_invalidated: Option<bool>,
    /// Exclude retired memories (superseded by a derivation). Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_retired: Option<bool>,
    /// Filter by external ref kind and optionally value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_filter: Option<RefFilter>,
    /// Context-faceted retrieval. Each supplied facet intersects with the row's
    /// `applies_to_*` column via array containment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<AppliesTo>,
    /// Include raw-layer types (observation, episode, decision). Default false.
    /// When false and `memory_types` is unset, only consolidated types are returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_raw: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub id: Uuid,
    pub snippet: String,
    pub similarity: f32,
    pub score: f32,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_refs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

pub type SearchOutput = Envelope<SearchHit>;

#[tracing::instrument(
    name = "tool.search_memories",
    skip(pool, embedder, args),
    fields(profile = %args.profile, k = ?args.k, has_tags = args.tags.is_some()),
)]
pub async fn handle(
    pool: &PgPool,
    embedder: Arc<Embedder>,
    query_log_enabled: bool,
    search_cfg: &SearchConfig,
    args: SearchArgs,
) -> Result<SearchOutput> {
    let search_start = std::time::Instant::now();
    // Destructure up front so we can move `query` into spawn_blocking
    // without cloning and still use the other fields afterward.
    let SearchArgs {
        profile,
        query,
        k,
        max_tokens,
        tags,
        min_similarity,
        include_content,
        memory_types,
        exclude_invalidated,
        exclude_retired,
        ref_filter,
        applies_to,
        include_raw,
    } = args;
    let include_content = include_content.unwrap_or(false);
    let exclude_invalidated = exclude_invalidated.unwrap_or(true);
    let exclude_retired = exclude_retired.unwrap_or(true);
    let include_raw = include_raw.unwrap_or(false);
    let applies_to = applies_to.unwrap_or_default();

    validate::profile(TOOL, &profile)?;
    validate::content_byte_length(TOOL, &query)?;
    if query.is_empty() {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "query".to_string(),
            constraint: "length >= 1".to_string(),
            received: Some(serde_json::json!("")),
            next_action: "Pass a non-empty query string.".to_string(),
        });
    }

    let k = k.unwrap_or(DEFAULT_K);
    validate::k(TOOL, k)?;
    if let Some(cap) = max_tokens {
        validate::max_tokens(TOOL, cap)?;
    }
    let tags = tags.unwrap_or_default();
    validate::tags(TOOL, &tags)?;
    let memory_types = match memory_types {
        Some(mt) if !mt.is_empty() => mt,
        _ if include_raw => vec![],
        _ => CONSOLIDATED_TYPES.iter().map(|s| s.to_string()).collect(),
    };
    validate::memory_types(TOOL, &memory_types)?;
    let min_similarity = min_similarity.unwrap_or(0.0);
    validate::min_similarity(TOOL, min_similarity)?;

    let ref_filter_json = if let Some(ref rf) = ref_filter {
        validate::ref_filter(TOOL, rf)?;
        let mut obj = serde_json::json!({"kind": rf.kind});
        if let Some(ref val) = rf.ref_value {
            obj["ref"] = serde_json::json!(val);
        }
        Some(serde_json::json!([obj]))
    } else {
        None
    };

    let use_hybrid = search_cfg.rrf_fts || search_cfg.rrf_sparse;
    let fetch_k = if search_cfg.dedup_field.is_some() {
        k * search_cfg.dedup_fetch_factor
    } else {
        k
    };

    let at_domains = applies_to.domains.unwrap_or_default();
    let at_skills = applies_to.skills.unwrap_or_default();
    let at_projects = applies_to.projects.unwrap_or_default();
    let at_situations = applies_to.situations.unwrap_or_default();

    let embed_out = embedder.embed_full(&query, "search_memories").await?;
    let query_vec = Vector::from(embed_out.dense.clone());

    let (hits, total_available) = if use_hybrid {
        crate::retrieval::search_hybrid(
            pool,
            &crate::retrieval::HybridSearchParams {
                profile: &profile,
                query_embed: &embed_out,
                k: fetch_k,
                tags: &tags,
                memory_types: &memory_types,
                recency_weight: search_cfg.recency_weight,
                recency_half_life_days: search_cfg.recency_half_life_days,
                search_cfg,
                query_text: &query,
                exclude_invalidated,
                exclude_retired,
                ref_filter_json: ref_filter_json.as_ref(),
                applies_to_domains: &at_domains,
                applies_to_skills: &at_skills,
                applies_to_projects: &at_projects,
                applies_to_situations: &at_situations,
            },
        )
        .await?
    } else {
        db::search_by_embedding(
            pool,
            &db::SearchParams {
                profile: &profile,
                query: &query_vec,
                k: fetch_k,
                tags: &tags,
                memory_types: &memory_types,
                min_similarity,
                recency_weight: search_cfg.recency_weight,
                recency_half_life_days: search_cfg.recency_half_life_days,
                exclude_invalidated,
                exclude_retired,
                ref_filter_json: ref_filter_json.as_ref(),
                applies_to_domains: &at_domains,
                applies_to_skills: &at_skills,
                applies_to_projects: &at_projects,
                applies_to_situations: &at_situations,
            },
        )
        .await?
    };

    let hits = if let Some(ref field) = search_cfg.dedup_field {
        dedup_by_field(hits, field, k as usize)
    } else {
        hits
    };

    let mut hits = hits;
    if !use_hybrid {
        crate::retrieval::apply_type_weights(&mut hits, &search_cfg.type_weights);
    }

    let candidates: Vec<SearchHit> = hits
        .into_iter()
        .map(|hit| SearchHit {
            id: hit.id,
            snippet: prefix_chars(&hit.content, SNIPPET_CHARS),
            similarity: hit.similarity,
            score: hit.score,
            event_time: hit.event_time,
            record_time: hit.record_time,
            tags: hit.tags,
            content: if include_content {
                Some(hit.content)
            } else {
                None
            },
            metadata: if include_content { hit.metadata } else { None },
            memory_type: hit.memory_type,
            source: hit.source,
            external_refs: hit.external_refs,
            confidence: hit.confidence,
        })
        .collect();

    let total_available_u64 = u64::try_from(total_available).unwrap_or(0);
    let (results, mut truncated) = apply_budget(candidates, max_tokens, total_available_u64);

    // `truncated` reflects only budget truncation and k-limit. If the DB
    // returned exactly k rows, the SQL LIMIT was hit — flag `truncated`.
    // `total_available` is still reported in the envelope for informational
    // purposes but no longer drives agent pagination decisions.
    if !truncated && results.len() == k as usize {
        truncated = true;
    }

    // Build the envelope, then overwrite `budget_spent_tokens` with the
    // estimator applied to the fully-assembled payload. This avoids a
    // magic-constant overhead and matches what the wire will carry.
    let mut envelope = Envelope::new(results, truncated, Some(total_available_u64), 0);
    envelope.budget_spent_tokens = estimate_tokens(&envelope);

    // Fire-and-forget query log for retrieval research.
    if query_log_enabled {
        let latency_ms = search_start.elapsed().as_millis() as i64;
        let result_ids: Vec<Uuid> = envelope.results.iter().map(|h| h.id).collect();
        let result_scores: Vec<f32> = envelope.results.iter().map(|h| h.score).collect();
        let log_pool = pool.clone();
        let log_profile = profile.clone();
        let log_query = query.clone();
        let log_embedding = query_vec.clone();
        let log_tags = tags.clone();
        let log_memory_types = memory_types.clone();
        let log_total = Some(total_available);
        let log_truncated = envelope.truncated;
        tokio::spawn(async move {
            if let Err(e) = db::insert_query_log(
                &log_pool,
                &db::QueryLogInput {
                    profile: &log_profile,
                    query_text: &log_query,
                    embedding: &log_embedding,
                    k,
                    min_similarity,
                    tags: &log_tags,
                    memory_types: &log_memory_types,
                    result_ids: &result_ids,
                    result_scores: &result_scores,
                    total_available: log_total,
                    truncated: log_truncated,
                    latency_ms,
                },
            )
            .await
            {
                tracing::warn!("query log insert failed: {e}");
            }
        });
    }

    Ok(envelope)
}

/// Keep the highest-scoring hit per unique value of `metadata[field]`.
/// Hits without the field pass through unconditionally (no key = always unique).
/// Input must be sorted by similarity descending (DB/RRF order).
fn dedup_by_field(hits: Vec<db::SearchHit>, field: &str, k: usize) -> Vec<db::SearchHit> {
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(k);
    for hit in hits {
        let dominated = hit
            .metadata
            .as_ref()
            .and_then(|m| m.get(field))
            .and_then(|v| v.as_str())
            .is_some_and(|key| !seen.insert(key.to_string()));
        if !dominated {
            result.push(hit);
            if result.len() >= k {
                break;
            }
        }
    }
    result
}

/// Truncate `candidates` to fit `max_tokens`.
///
/// Returns `(results, truncated)`. When `max_tokens` is `None`, every
/// candidate is kept and `truncated` is `false`. When set, the first
/// candidate is always included (an empty envelope is less useful than an
/// oversize first result); subsequent candidates are rejected if they would
/// push the *full envelope's* token count over `cap`. The cap accounts for
/// envelope wrapper fields (`results`, `truncated`, `total_available`,
/// `budget_spent_tokens`), not just hit payloads — so the number the caller
/// sees is the number the cap enforced against.
fn apply_budget(
    candidates: Vec<SearchHit>,
    max_tokens: Option<u64>,
    total_available: u64,
) -> (Vec<SearchHit>, bool) {
    let Some(cap) = max_tokens else {
        return (candidates, false);
    };
    // Seed `spent` with the fixed overhead of an empty envelope — the
    // wrapper JSON exists even when `results` is `[]`, and callers who
    // pass a tight cap should see truncation triggered honestly.
    // Use the real `total_available` so the seed envelope matches the shape
    // of the final envelope (Some(n) vs None changes the serialised length).
    let overhead_envelope = Envelope::new(Vec::<SearchHit>::new(), false, Some(total_available), 0);
    let overhead = estimate_tokens(&overhead_envelope);
    let mut results: Vec<SearchHit> = Vec::with_capacity(candidates.len());
    let mut spent = overhead;
    let mut truncated = false;
    for candidate in candidates {
        let candidate_tokens = estimate_tokens(&candidate);
        if !results.is_empty() && spent.saturating_add(candidate_tokens) > cap {
            truncated = true;
            break;
        }
        spent = spent.saturating_add(candidate_tokens);
        results.push(candidate);
    }
    (results, truncated)
}

/// Verbatim char-prefix of `s` up to `max_chars` Unicode scalar values.
/// No ellipsis; if `s` is shorter, returns it unchanged.
pub(crate) fn prefix_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn prefix_chars_truncates_at_boundary() {
        let s = "abcdefg";
        assert_eq!(prefix_chars(s, 4), "abcd");
    }

    #[test]
    fn prefix_chars_returns_whole_when_short() {
        assert_eq!(prefix_chars("ab", 10), "ab");
    }

    #[test]
    fn prefix_chars_handles_multi_byte_unicode() {
        let s = "αβγδε"; // 5 chars, 10 bytes
        assert_eq!(prefix_chars(s, 3), "αβγ");
    }

    fn hit(snippet: &str) -> SearchHit {
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        SearchHit {
            id: Uuid::now_v7(),
            snippet: snippet.to_string(),
            similarity: 0.9,
            score: 0.9,
            event_time: t,
            record_time: t,
            tags: vec![],
            content: None,
            metadata: None,
            memory_type: "observation".to_string(),
            source: None,
            external_refs: None,
            confidence: None,
        }
    }

    #[test]
    fn apply_budget_none_keeps_all() {
        let candidates = vec![hit("a"), hit("b"), hit("c")];
        let (results, truncated) = apply_budget(candidates, None, 0);
        assert_eq!(results.len(), 3);
        assert!(!truncated);
    }

    #[test]
    fn apply_budget_tight_keeps_at_least_one() {
        // Any single hit is larger than 1 token. We still get the first.
        let (results, truncated) = apply_budget(vec![hit("first"), hit("second")], Some(1), 0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "first");
        assert!(truncated);
    }

    #[test]
    fn apply_budget_stops_before_overflow() {
        // Estimate each hit, pick a cap that fits exactly one.
        let h1 = hit("one");
        let h2 = hit("two");
        let per = estimate_tokens(&h1);
        let cap = per; // fits the first, blocks the second
        let (results, truncated) = apply_budget(vec![h1, h2], Some(cap), 0);
        assert_eq!(results.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn apply_budget_fits_full_list_when_cap_is_ample() {
        let candidates = vec![hit("a"), hit("b"), hit("c")];
        let total: u64 = candidates.iter().map(estimate_tokens).sum();
        let (results, truncated) = apply_budget(candidates, Some(total + 100), 0);
        assert_eq!(results.len(), 3);
        assert!(!truncated);
    }
}
