//! Postgres + pgvector repo.
//!
//! Runtime-checked queries (`sqlx::query`/`query_as`) so a fresh clone
//! can `cargo build` without a live database — rationale in
//! `docs/starting-shape.md` § sqlx mode.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sqlx::FromRow;
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

use crate::config::Config;
use crate::error::{ChittaError, Result};
use crate::facets::Facets;

/// One row of `memories`. Mirrors the v0 schema (0001_init.sql).
#[derive(Debug, Clone, FromRow)]
pub struct MemoryRow {
    pub id: Uuid,
    pub profile: String,
    pub content: String,
    pub embedding: Option<Vector>,
    pub sparse_embedding: Option<serde_json::Value>,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub idempotency_key: String,
    pub source: Option<String>,
    pub memory_type: String,
    pub tags: Vec<String>,
    pub external_refs: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
    #[sqlx(flatten)]
    pub facets: Facets,
    pub superseded_by: Option<Uuid>,
    pub confidence: Option<f32>,
    pub reinforcement_count: i32,
    pub last_reinforced_at: Option<DateTime<Utc>>,
    pub invalidated_at: Option<DateTime<Utc>>,
}

/// One hit from an ANN search. `similarity` is the raw cosine score
/// (`1 - cosine_distance`). `score` is the final composite after any
/// recency boost, RRF fusion, and type-weight multiplier — used for ranking.
#[derive(Debug, Clone, FromRow)]
pub struct SearchHit {
    pub id: Uuid,
    pub content: String,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    pub similarity: f32,
    #[sqlx(default)]
    pub score: f32,
    pub source: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub memory_type: String,
    pub external_refs: Option<serde_json::Value>,
    pub confidence: Option<f32>,
}

pub struct MemoryPatch<'a> {
    pub profile: &'a str,
    pub id: Uuid,
    pub content: Option<&'a str>,
    pub embedding: Option<&'a Vector>,
    pub sparse_embedding: Option<&'a serde_json::Value>,
    pub tags: Option<&'a [String]>,
    pub metadata: Option<&'a serde_json::Value>,
    pub memory_type: Option<&'a str>,
    pub external_refs: Option<&'a serde_json::Value>,
}

pub struct SearchParams<'a> {
    pub profile: &'a str,
    pub query: &'a Vector,
    pub k: i64,
    pub tags: &'a [String],
    pub memory_types: &'a [String],
    pub min_similarity: f32,
    pub recency_weight: f32,
    pub recency_half_life_days: f32,
    pub exclude_invalidated: bool,
    pub exclude_superseded: bool,
    pub ref_filter_json: Option<&'a serde_json::Value>,
    pub facets: &'a Facets,
}

pub struct QueryLogInput<'a> {
    pub profile: &'a str,
    pub query_text: &'a str,
    pub embedding: &'a Vector,
    pub sparse_embedding: Option<&'a serde_json::Value>,
    pub k: i64,
    pub min_similarity: f32,
    pub tags: &'a [String],
    pub memory_types: &'a [String],
    pub result_ids: &'a [Uuid],
    pub result_scores: &'a [f32],
    pub total_available: Option<i64>,
    pub truncated: bool,
    pub latency_ms: i64,
}

pub async fn connect(cfg: &Config) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(cfg.db_max_connections)
        .acquire_timeout(std::time::Duration::from_secs(cfg.db_acquire_timeout_secs))
        .idle_timeout(std::time::Duration::from_secs(cfg.db_idle_timeout_secs))
        .connect(&cfg.database_url)
        .await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// The SQLSTATE code Postgres raises on unique-constraint violation.
/// We intercept it on `insert_memory` to implement the idempotency contract.
const PG_UNIQUE_VIOLATION: &str = "23505";

/// Attempt to insert. On `(profile, idempotency_key)` conflict, fetch and
/// return the existing row — this is the idempotency contract (Principle 6).
///
/// Returns `(row, idempotent_replay)`.
pub async fn insert_or_fetch_memory(pool: &PgPool, new: &MemoryRow) -> Result<(MemoryRow, bool)> {
    let insert_result = sqlx::query_as::<_, MemoryRow>(
        r#"
        INSERT INTO memories
            (id, profile, content, embedding, sparse_embedding,
             event_time, record_time, idempotency_key, source, memory_type,
             tags, external_refs, metadata,
             applies_to_domains, applies_to_skills, applies_to_projects, applies_to_situations,
             superseded_by, confidence, reinforcement_count, last_reinforced_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
        RETURNING *
        "#,
    )
    .bind(new.id)
    .bind(&new.profile)
    .bind(&new.content)
    .bind(&new.embedding)
    .bind(&new.sparse_embedding)
    .bind(new.event_time)
    .bind(new.record_time)
    .bind(&new.idempotency_key)
    .bind(&new.source)
    .bind(&new.memory_type)
    .bind(&new.tags)
    .bind(&new.external_refs)
    .bind(&new.metadata)
    .bind(&new.facets.domains)
    .bind(&new.facets.skills)
    .bind(&new.facets.projects)
    .bind(&new.facets.situations)
    .bind(new.superseded_by)
    .bind(new.confidence)
    .bind(new.reinforcement_count)
    .bind(new.last_reinforced_at)
    .fetch_one(pool)
    .await;

    match insert_result {
        Ok(row) => Ok((row, false)),
        Err(e) => {
            if is_unique_violation(&e) {
                let existing = find_by_idempotency_key(pool, &new.profile, &new.idempotency_key)
                    .await?
                    .ok_or_else(|| {
                        ChittaError::Internal(
                            "unique violation without recoverable row".to_string(),
                        )
                    })?;
                Ok((existing, true))
            } else {
                Err(e.into())
            }
        }
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = e {
        db.code().as_deref() == Some(PG_UNIQUE_VIOLATION)
    } else {
        false
    }
}

pub async fn find_by_idempotency_key(
    pool: &PgPool,
    profile: &str,
    idempotency_key: &str,
) -> Result<Option<MemoryRow>> {
    let row = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT * FROM memories
        WHERE profile = $1 AND idempotency_key = $2
          AND invalidated_at IS NULL
        "#,
    )
    .bind(profile)
    .bind(idempotency_key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_memory_by_id(pool: &PgPool, profile: &str, id: Uuid) -> Result<Option<MemoryRow>> {
    let row = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT * FROM memories
        WHERE profile = $1 AND id = $2
          AND invalidated_at IS NULL
        "#,
    )
    .bind(profile)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Update a memory's content and/or tags. Uses COALESCE so only provided
/// fields are overwritten. When content changes, the caller must supply a new
/// embedding. `record_time` is never touched (bi-temporal invariant).
///
/// Returns the updated row, or `None` if the `(profile, id)` pair does not
/// exist (caller turns that into `NotFound`).
pub async fn update_memory(pool: &PgPool, patch: &MemoryPatch<'_>) -> Result<Option<MemoryRow>> {
    let row = sqlx::query_as::<_, MemoryRow>(
        r#"
        UPDATE memories
        SET content          = COALESCE($3, content),
            embedding        = COALESCE($4, embedding),
            sparse_embedding = COALESCE($5, sparse_embedding),
            tags             = COALESCE($6, tags),
            metadata         = COALESCE($7, metadata),
            memory_type      = COALESCE($8, memory_type),
            external_refs    = COALESCE($9, external_refs)
        WHERE profile = $1 AND id = $2
          AND invalidated_at IS NULL
        RETURNING *
        "#,
    )
    .bind(patch.profile)
    .bind(patch.id)
    .bind(patch.content)
    .bind(patch.embedding)
    .bind(patch.sparse_embedding)
    .bind(patch.tags)
    .bind(patch.metadata)
    .bind(patch.memory_type)
    .bind(patch.external_refs)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Soft-delete a memory by setting `invalidated_at`. Returns `true` if a
/// row was invalidated. Already-invalidated rows are not matched.
pub async fn delete_memory(pool: &PgPool, profile: &str, id: Uuid) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE memories
        SET invalidated_at = now()
        WHERE profile = $1 AND id = $2
          AND invalidated_at IS NULL
        "#,
    )
    .bind(profile)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// List recent memories ordered by `record_time DESC`. When `tags` is
/// non-empty, only rows sharing at least one tag are returned (OR match).
pub async fn list_recent(
    pool: &PgPool,
    profile: &str,
    limit: i64,
    tags: &[String],
    memory_types: &[String],
) -> Result<Vec<MemoryRow>> {
    let rows = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT * FROM memories
        WHERE profile = $1
          AND invalidated_at IS NULL
          AND ($3::text[] = '{}' OR tags && $3)
          AND ($4::text[] = '{}' OR memory_type = ANY($4))
        ORDER BY record_time DESC
        LIMIT $2
        "#,
    )
    .bind(profile)
    .bind(limit)
    .bind(tags)
    .bind(memory_types)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Count all memories in a profile (regardless of tags).
pub async fn count_profile(pool: &PgPool, profile: &str) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)::bigint FROM memories WHERE profile = $1 AND invalidated_at IS NULL
        "#,
    )
    .bind(profile)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// List recent + count in a single transaction for consistency.
pub async fn list_recent_with_count(
    pool: &PgPool,
    profile: &str,
    limit: i64,
    tags: &[String],
    memory_types: &[String],
) -> Result<(Vec<MemoryRow>, i64)> {
    let mut tx = pool.begin().await?;

    let rows = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT * FROM memories
        WHERE profile = $1
          AND invalidated_at IS NULL
          AND ($3::text[] = '{}' OR tags && $3)
          AND ($4::text[] = '{}' OR memory_type = ANY($4))
        ORDER BY record_time DESC
        LIMIT $2
        "#,
    )
    .bind(profile)
    .bind(limit)
    .bind(tags)
    .bind(memory_types)
    .fetch_all(&mut *tx)
    .await?;

    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)::bigint FROM memories
        WHERE profile = $1
          AND invalidated_at IS NULL
          AND ($2::text[] = '{}' OR tags && $2)
          AND ($3::text[] = '{}' OR memory_type = ANY($3))
        "#,
    )
    .bind(profile)
    .bind(tags)
    .bind(memory_types)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((rows, count))
}

/// Minimum `hnsw.ef_search` used for every semantic query. pgvector's
/// default is 40, which both caps HNSW candidate breadth and undershoots
/// any WHERE post-filter that rejects most of those candidates. We raise
/// the floor so (a) `min_similarity`/tag filters don't silently shrink
/// result counts and (b) `LIMIT k` actually returns ~k rows when matches
/// exist. Capped at `HNSW_EF_SEARCH_MAX` to bound per-query work.
const HNSW_EF_SEARCH_MIN: i64 = 200;
const HNSW_EF_SEARCH_MAX: i64 = 1000;

/// Semantic search with optional tag filter and similarity floor.
///
/// Tag match is OR: a row passes if it shares at least one tag with `tags`.
/// When `tags` is empty, no tag filter is applied.
///
/// Returns `(hits, total_available)`. `total_available` is the count of rows
/// matching **profile + tag filter** — it deliberately ignores
/// `min_similarity`, because counting rows above a cosine threshold would
/// require scanning every embedding, defeating the ANN index. The agent gets
/// a truthful ceiling on candidate breadth; the similarity-gated subset is
/// what `results` reports.
///
/// Runs inside a short transaction so `SET LOCAL hnsw.ef_search` scopes to
/// the ANN query only and doesn't leak to other pool users.
pub async fn search_by_embedding(
    pool: &PgPool,
    p: &SearchParams<'_>,
) -> Result<(Vec<SearchHit>, i64)> {
    // ef_search is an integer GUC; SET LOCAL does not accept bind params,
    // so we clamp to a known-safe integer range and format inline. k is
    // already range-checked by the validator; the clamp below is belt +
    // suspenders against a future caller reaching this fn with a bad k.
    let ef_search = (p.k.max(1) * 4).clamp(HNSW_EF_SEARCH_MIN, HNSW_EF_SEARCH_MAX);
    let mut tx = pool.begin().await?;

    let facet_clauses = Facets::sql_filter_clauses(7);
    let count_sql = format!(
        "SELECT count(*)::bigint FROM memories \
         WHERE profile = $1 \
         AND (NOT $4 OR invalidated_at IS NULL) \
         AND (NOT $5 OR superseded_by IS NULL) \
         AND ($2::text[] = '{{}}' OR tags && $2) \
         AND ($3::text[] = '{{}}' OR memory_type = ANY($3)) \
         AND ($6::jsonb IS NULL OR external_refs @> $6){facet_clauses}"
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(p.profile)
        .bind(p.tags)
        .bind(p.memory_types)
        .bind(p.exclude_invalidated)
        .bind(p.exclude_superseded)
        .bind(p.ref_filter_json)
        .bind(&p.facets.domains)
        .bind(&p.facets.skills)
        .bind(&p.facets.projects)
        .bind(&p.facets.situations)
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query(&format!("set local hnsw.ef_search = {ef_search}"))
        .execute(&mut *tx)
        .await?;

    let use_recency = p.recency_weight > 0.0;
    let fetch_limit = if use_recency { p.k * 2 } else { p.k };

    let search_facet_clauses = Facets::sql_filter_clauses(10);
    let search_sql = format!(
        "SELECT id, content, event_time, record_time, tags, \
         (1.0 - (embedding <=> $2))::real AS similarity, \
         source, metadata, memory_type, external_refs, confidence \
         FROM memories \
         WHERE profile = $1 \
         AND (NOT $7 OR invalidated_at IS NULL) \
         AND (NOT $8 OR superseded_by IS NULL) \
         AND ($3::text[] = '{{}}' OR tags && $3) \
         AND ($6::text[] = '{{}}' OR memory_type = ANY($6)) \
         AND (1.0 - (embedding <=> $2))::real >= $4 \
         AND ($9::jsonb IS NULL OR external_refs @> $9){search_facet_clauses} \
         ORDER BY embedding <=> $2 \
         LIMIT $5"
    );
    let hits = sqlx::query_as::<_, SearchHit>(&search_sql)
        .bind(p.profile)
        .bind(p.query)
        .bind(p.tags)
        .bind(p.min_similarity)
        .bind(fetch_limit)
        .bind(p.memory_types)
        .bind(p.exclude_invalidated)
        .bind(p.exclude_superseded)
        .bind(p.ref_filter_json)
        .bind(&p.facets.domains)
        .bind(&p.facets.skills)
        .bind(&p.facets.projects)
        .bind(&p.facets.situations)
        .fetch_all(&mut *tx)
        .await?;

    let mut hits: Vec<SearchHit> = hits
        .into_iter()
        .map(|mut h| {
            h.score = h.similarity;
            h
        })
        .collect();

    if use_recency {
        let now = Utc::now();
        let hl_secs = (p.recency_half_life_days as f64) * 86400.0;
        for h in &mut hits {
            let age_secs = (now - h.event_time).num_seconds().max(0) as f64;
            let recency_factor = (-age_secs / hl_secs).exp() as f32;
            h.score *= 1.0 + p.recency_weight * recency_factor;
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(p.k as usize);
    }

    tx.commit().await?;
    Ok((hits, total))
}

pub async fn fetch_sparse_embeddings(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, HashMap<u32, f32>)>> {
    let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        r#"
        SELECT id, sparse_embedding
        FROM memories
        WHERE id = ANY($1)
          AND sparse_embedding IS NOT NULL
        "#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;

    let mut result = Vec::with_capacity(rows.len());
    for (id, json) in rows {
        let map: HashMap<u32, f32> = match serde_json::from_value(json) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%id, "corrupt sparse_embedding JSONB, treating as empty: {e}");
                HashMap::new()
            }
        };
        result.push((id, map));
    }
    Ok(result)
}

pub async fn fetch_search_hits_by_ids(
    pool: &PgPool,
    profile: &str,
    ids: &[Uuid],
) -> Result<Vec<SearchHit>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    let rows = sqlx::query_as::<_, SearchHit>(
        r#"
        SELECT id, content, event_time, record_time, tags,
               1.0::real AS similarity, source, metadata, memory_type, external_refs, confidence
        FROM memories
        WHERE profile = $1
          AND invalidated_at IS NULL
          AND id = ANY($2)
        "#,
    )
    .bind(profile)
    .bind(ids)
    .fetch_all(pool)
    .await?;

    // Preserve the ordering of the input IDs (RRF rank order).
    let pos: HashMap<Uuid, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut sorted = rows;
    sorted.sort_by_key(|h| pos.get(&h.id).copied().unwrap_or(usize::MAX));
    Ok(sorted)
}

/// One row from the `query_log` table. Used by the replay subcommand.
#[derive(Debug, Clone, FromRow)]
pub struct QueryLogEntry {
    pub id: i64,
    pub profile: String,
    pub query_text: String,
    pub embedding: Vector,
    pub sparse_embedding: Option<serde_json::Value>,
    pub k: i32,
    pub min_similarity: f32,
    pub tags: Vec<String>,
    pub memory_types: Vec<String>,
    pub result_ids: Vec<Uuid>,
    pub result_scores: Vec<f32>,
    pub total_available: Option<i64>,
    pub truncated: bool,
    pub latency_ms: i32,
    pub created_at: DateTime<Utc>,
}

/// Read query_log entries, optionally filtered by profile, ordered by
/// `created_at DESC` (most recent first), limited to `limit` rows.
pub async fn read_query_log(
    pool: &PgPool,
    profile: Option<&str>,
    limit: i64,
) -> Result<Vec<QueryLogEntry>> {
    let rows = sqlx::query_as::<_, QueryLogEntry>(
        r#"
        SELECT id, profile, query_text, embedding, sparse_embedding, k, min_similarity,
               tags, memory_types, result_ids, result_scores, total_available, truncated,
               latency_ms, created_at
        FROM query_log
        WHERE ($1::text IS NULL OR profile = $1)
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(profile)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Append-only insert into `query_log`. Fire-and-forget from the search
/// handler — errors are logged but never propagated to the caller.
pub async fn insert_query_log(pool: &PgPool, e: &QueryLogInput<'_>) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO query_log
            (profile, query_text, embedding, sparse_embedding, k, min_similarity,
             tags, memory_types, result_ids, result_scores, total_available, truncated,
             latency_ms)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        "#,
    )
    .bind(e.profile)
    .bind(e.query_text)
    .bind(e.embedding)
    .bind(e.sparse_embedding)
    .bind(e.k as i32)
    .bind(e.min_similarity)
    .bind(e.tags)
    .bind(e.memory_types)
    .bind(e.result_ids)
    .bind(e.result_scores)
    .bind(e.total_available)
    .bind(e.truncated)
    .bind(e.latency_ms as i32)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a memory and its derivations atomically in a single transaction.
/// On idempotency conflict, returns the existing row (derivations are not re-inserted).
pub async fn insert_memory_with_derivations(
    pool: &PgPool,
    new: &MemoryRow,
    derivations: &[(Uuid, String)],
) -> Result<(MemoryRow, bool)> {
    let mut tx = pool.begin().await?;

    let insert_result = sqlx::query_as::<_, MemoryRow>(
        r#"
        INSERT INTO memories
            (id, profile, content, embedding, sparse_embedding,
             event_time, record_time, idempotency_key, source, memory_type,
             tags, external_refs, metadata,
             applies_to_domains, applies_to_skills, applies_to_projects, applies_to_situations,
             superseded_by, confidence, reinforcement_count, last_reinforced_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
        RETURNING *
        "#,
    )
    .bind(new.id)
    .bind(&new.profile)
    .bind(&new.content)
    .bind(&new.embedding)
    .bind(&new.sparse_embedding)
    .bind(new.event_time)
    .bind(new.record_time)
    .bind(&new.idempotency_key)
    .bind(&new.source)
    .bind(&new.memory_type)
    .bind(&new.tags)
    .bind(&new.external_refs)
    .bind(&new.metadata)
    .bind(&new.facets.domains)
    .bind(&new.facets.skills)
    .bind(&new.facets.projects)
    .bind(&new.facets.situations)
    .bind(new.superseded_by)
    .bind(new.confidence)
    .bind(new.reinforcement_count)
    .bind(new.last_reinforced_at)
    .fetch_one(&mut *tx)
    .await;

    let stored = match insert_result {
        Ok(row) => row,
        Err(e) => {
            if is_unique_violation(&e) {
                tx.rollback().await.ok();
                let existing = find_by_idempotency_key(pool, &new.profile, &new.idempotency_key)
                    .await?
                    .ok_or_else(|| {
                        ChittaError::Internal(
                            "unique violation without recoverable row".to_string(),
                        )
                    })?;
                return Ok((existing, true));
            }
            return Err(e.into());
        }
    };

    for (source_id, derivation_type) in derivations {
        sqlx::query(
            r#"
            INSERT INTO derivations (derived_id, source_id, derivation_type)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(stored.id)
        .bind(source_id)
        .bind(derivation_type)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok((stored, false))
}

// ── derivations (migration 0009) ────────────────────────────────────

#[derive(Debug, Clone, FromRow)]
pub struct DerivationRow {
    pub id: Uuid,
    pub derived_id: Uuid,
    pub source_id: Uuid,
    pub derivation_type: String,
    pub created_at: DateTime<Utc>,
}

pub async fn insert_derivation(
    pool: &PgPool,
    derived_id: Uuid,
    source_id: Uuid,
    derivation_type: &str,
) -> Result<DerivationRow> {
    let row = sqlx::query_as::<_, DerivationRow>(
        r#"
        INSERT INTO derivations (derived_id, source_id, derivation_type)
        VALUES ($1, $2, $3)
        RETURNING id, derived_id, source_id, derivation_type, created_at
        "#,
    )
    .bind(derived_id)
    .bind(source_id)
    .bind(derivation_type)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get_derivations_for(pool: &PgPool, derived_id: Uuid) -> Result<Vec<DerivationRow>> {
    let rows = sqlx::query_as::<_, DerivationRow>(
        r#"
        SELECT id, derived_id, source_id, derivation_type, created_at
        FROM derivations
        WHERE derived_id = $1
        ORDER BY created_at
        "#,
    )
    .bind(derived_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn supersede_memory(pool: &PgPool, old_id: Uuid, new_id: Uuid) -> Result<DerivationRow> {
    let mut tx = pool.begin().await?;

    sqlx::query("UPDATE memories SET superseded_by = $1 WHERE id = $2")
        .bind(new_id)
        .bind(old_id)
        .execute(&mut *tx)
        .await?;

    let derivation = sqlx::query_as::<_, DerivationRow>(
        r#"
        INSERT INTO derivations (derived_id, source_id, derivation_type)
        VALUES ($1, $2, 'supersedes')
        RETURNING id, derived_id, source_id, derivation_type, created_at
        "#,
    )
    .bind(new_id)
    .bind(old_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(derivation)
}

pub async fn get_derived_from(pool: &PgPool, source_id: Uuid) -> Result<Vec<DerivationRow>> {
    let rows = sqlx::query_as::<_, DerivationRow>(
        r#"
        SELECT id, derived_id, source_id, derivation_type, created_at
        FROM derivations
        WHERE source_id = $1
        ORDER BY created_at
        "#,
    )
    .bind(source_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── reflect_runs ────────────────────────────────────────────────────

#[derive(Debug, Clone, FromRow)]
pub struct ReflectRunRow {
    pub id: Uuid,
    pub profile: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub rows_scanned: i32,
    pub summary: Option<serde_json::Value>,
}

pub async fn last_reflect_run(pool: &PgPool, profile: &str) -> Result<Option<ReflectRunRow>> {
    let row = sqlx::query_as::<_, ReflectRunRow>(
        r#"
        SELECT id, profile, started_at, completed_at, rows_scanned, summary
        FROM reflect_runs
        WHERE profile = $1 AND completed_at IS NOT NULL
        ORDER BY completed_at DESC
        LIMIT 1
        "#,
    )
    .bind(profile)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn insert_reflect_run(
    pool: &PgPool,
    profile: &str,
    rows_scanned: i32,
    summary: Option<serde_json::Value>,
) -> Result<ReflectRunRow> {
    let now = Utc::now();
    let row = sqlx::query_as::<_, ReflectRunRow>(
        r#"
        INSERT INTO reflect_runs (profile, started_at, completed_at, rows_scanned, summary)
        VALUES ($1, $2, $2, $3, $4)
        RETURNING id, profile, started_at, completed_at, rows_scanned, summary
        "#,
    )
    .bind(profile)
    .bind(now)
    .bind(rows_scanned)
    .bind(summary)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Fetch raw rows (observation, episode, decision) since a given timestamp.
/// When `since` is None, returns all raw rows.
pub async fn fetch_raw_since(
    pool: &PgPool,
    profile: &str,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<MemoryRow>> {
    let since = since.unwrap_or(DateTime::UNIX_EPOCH);
    let rows = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT id, profile, content, embedding, sparse_embedding,
               event_time, record_time, idempotency_key, source,
               memory_type, tags, external_refs, metadata,
               applies_to_domains, applies_to_skills,
               applies_to_projects, applies_to_situations,
               superseded_by, confidence, reinforcement_count,
               last_reinforced_at, invalidated_at
        FROM memories
        WHERE profile = $1
          AND memory_type IN ('observation', 'episode', 'decision')
          AND invalidated_at IS NULL
          AND record_time > $2
        ORDER BY record_time ASC
        "#,
    )
    .bind(profile)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── profile candidates ─────────────────────────────────────────────

/// Over-fetch the top-100 active consolidated rows for tier-0 profile,
/// ordered by raw confidence DESC. The caller applies `effective_score`
/// decay and truncates to the final top-N.
pub async fn fetch_profile_candidates(pool: &PgPool, profile: &str) -> Result<Vec<MemoryRow>> {
    let rows = sqlx::query_as::<_, MemoryRow>(
        r#"
        SELECT id, profile, content, embedding, sparse_embedding,
               event_time, record_time, idempotency_key, source,
               memory_type, tags, external_refs, metadata,
               applies_to_domains, applies_to_skills,
               applies_to_projects, applies_to_situations,
               superseded_by, confidence, reinforcement_count,
               last_reinforced_at, invalidated_at
        FROM memories
        WHERE profile = $1
          AND memory_type IN ('trait', 'value', 'preference', 'pattern', 'mental_model')
          AND superseded_by IS NULL
          AND invalidated_at IS NULL
        ORDER BY confidence DESC NULLS LAST
        LIMIT 100
        "#,
    )
    .bind(profile)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
