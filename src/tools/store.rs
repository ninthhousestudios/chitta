//! `store_memory` handler.
//!
//! Validate → (look up by idempotency_key) → embed → insert → return.
//! On `(profile, idempotency_key)` conflict, returns the prior row with
//! `idempotent_replay: true` and does no new work (Principle 6).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, MemoryRow};
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::facets::Facets;
use crate::schema_hints;
use crate::tools::validate;
use crate::validators;
pub use crate::validators::DerivationInput;

const TOOL: &str = "store_memory";

/// Arguments for `store_memory`. Derives `JsonSchema` so rmcp can publish the
/// input schema on the wire — single source of truth for CLI and MCP.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct StoreArgs {
    /// Target profile namespace. 1-128 chars, `[a-zA-Z0-9_-]+` only.
    pub profile: String,
    /// Verbatim memory text. Stored as-is (Principle 1).
    pub content: String,
    /// Client-supplied dedup key. Same `(profile, idempotency_key)` returns
    /// the prior row with `idempotent_replay=true`.
    pub idempotency_key: String,
    /// When the subject happened. ISO-8601. Defaults to `record_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<DateTime<Utc>>,
    /// Optional tags. Up to 32, each 1-64 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Arbitrary structured data alongside the memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "schema_hints::nullable_object")]
    pub metadata: Option<serde_json::Value>,
    /// Memory type. Default: "observation".
    /// Valid: observation, episode, decision, trait, value, pattern, preference, mental_model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    /// External references. JSON array of `{"kind": "<type>", "ref": "<value>"}`.
    /// Kinds: file, commit, yojana_task, memory, url, session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "schema_hints::nullable_array")]
    pub external_refs: Option<serde_json::Value>,
    /// Context facets for retrieval scoping (domains, skills, projects, situations).
    #[serde(flatten)]
    pub facets: Facets,
    /// Confidence score (0.0–1.0). Typically NULL for raw-layer types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Which agent/harness wrote this memory (e.g. claude-code, codex, opencode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Derivations linking this memory to source memories.
    /// Required for `memory_type=episode` (at least one entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivations: Option<Vec<DerivationInput>>,
}

#[derive(Debug, Serialize)]
pub struct StoreOutput {
    pub id: Uuid,
    pub profile: String,
    pub content: String,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_refs: Option<serde_json::Value>,
    #[serde(flatten)]
    pub facets: Facets,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub idempotent_replay: bool,
}

#[tracing::instrument(
    name = "tool.store_memory",
    skip(pool, embedder, args),
    fields(profile = %args.profile, content_len = args.content.len()),
)]
pub async fn handle(
    pool: &PgPool,
    embedder: Arc<Embedder>,
    mut args: StoreArgs,
) -> Result<StoreOutput> {
    args.metadata = validators::coerce_json_string(args.metadata);
    args.external_refs = validators::coerce_json_string(args.external_refs);

    validate::profile(TOOL, &args.profile)?;
    validate::content_byte_length(TOOL, &args.content)?;
    validate::content_non_empty(TOOL, &args.content)?;
    validate::idempotency_key(TOOL, &args.idempotency_key)?;
    if let Some(et) = args.event_time {
        validate::event_time(TOOL, et)?;
    }
    let tags = args.tags.unwrap_or_default();
    validate::tags(TOOL, &tags)?;
    let memory_type = args
        .memory_type
        .unwrap_or_else(|| "observation".to_string());
    validators::memory_type(TOOL, &memory_type)?;
    if let Some(ref refs) = args.external_refs {
        validators::external_refs(TOOL, refs)?;
    }
    validators::episode_derivations(TOOL, &memory_type, &args.derivations)?;
    validators::validate_decision_metadata(TOOL, &memory_type, &args.metadata)?;

    if let Some(existing) =
        db::find_by_idempotency_key(pool, &args.profile, &args.idempotency_key).await?
    {
        return Ok(row_to_output(existing, true));
    }

    let content = args.content;
    let embed_out = embedder.embed_full(&content, "store_memory").await?;
    let sparse_embedding = if embed_out.sparse.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&embed_out.sparse).map_err(|e| {
            ChittaError::Internal(format!("failed to serialize sparse embedding: {e}"))
        })?)
    };

    let now = Utc::now();
    let event_time = args.event_time.unwrap_or(now);
    let row = MemoryRow {
        id: Uuid::now_v7(),
        profile: args.profile,
        content,
        embedding: Some(Vector::from(embed_out.dense)),
        sparse_embedding,
        event_time,
        record_time: now,
        idempotency_key: args.idempotency_key,
        source: args.source,
        memory_type,
        tags,
        external_refs: args.external_refs,
        metadata: args.metadata,
        facets: args.facets,
        superseded_by: None,
        confidence: args.confidence,
        reinforcement_count: 0,
        last_reinforced_at: None,
        invalidated_at: None,
    };

    let parsed_derivations: Vec<(Uuid, String)> = args
        .derivations
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            let uuid = Uuid::parse_str(&d.source_id).expect("validated above");
            (uuid, d.derivation_type)
        })
        .collect();

    if parsed_derivations.is_empty() {
        let (stored, replayed) = db::insert_or_fetch_memory(pool, &row).await?;
        Ok(row_to_output(stored, replayed))
    } else {
        let (stored, replayed) =
            db::insert_memory_with_derivations(pool, &row, &parsed_derivations).await?;
        Ok(row_to_output(stored, replayed))
    }
}

fn row_to_output(row: MemoryRow, replayed: bool) -> StoreOutput {
    StoreOutput {
        id: row.id,
        profile: row.profile,
        content: row.content,
        event_time: row.event_time,
        record_time: row.record_time,
        tags: row.tags,
        metadata: row.metadata,
        memory_type: row.memory_type,
        external_refs: row.external_refs,
        facets: row.facets,
        confidence: row.confidence,
        source: row.source,
        idempotent_replay: replayed,
    }
}
