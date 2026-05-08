//! `update_memory` handler.
//!
//! Validate → fetch existing → optionally re-embed → UPDATE → return.
//! `record_time` is never updated (bi-temporal invariant).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "update_memory";

/// Arguments for `update_memory`. At least one of `content` or `tags` must be
/// provided. `JsonSchema` is derived so rmcp exposes the same shape on the wire.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UpdateArgs {
    /// Profile scope.
    pub profile: String,
    /// Memory UUID to update.
    pub id: String,
    /// New content. If provided, the embedding is recomputed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// New tags. Replaces the existing tag list entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// New metadata. Replaces existing metadata entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// New memory type. Validates against allowed types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    /// New external references. Replaces existing refs entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_refs: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct UpdateOutput {
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
    pub re_embedded: bool,
}

#[tracing::instrument(
    name = "tool.update_memory",
    skip(pool, embedder, args),
    fields(profile = %args.profile, id = %args.id),
)]
pub async fn handle(
    pool: &PgPool,
    embedder: Arc<Embedder>,
    args: UpdateArgs,
) -> Result<UpdateOutput> {
    validate::profile(TOOL, &args.profile)?;
    let id = validate::parse_uuid(TOOL, "id", &args.id)?;

    if args.content.is_none()
        && args.tags.is_none()
        && args.metadata.is_none()
        && args.memory_type.is_none()
        && args.external_refs.is_none()
    {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "fields".to_string(),
            constraint: "at least one of content, tags, metadata, memory_type, or external_refs must be provided".to_string(),
            received: None,
            next_action: "Provide at least one field to update.".to_string(),
        });
    }

    if let Some(ref content) = args.content {
        validate::content_byte_length(TOOL, content)?;
        validate::content_non_empty(TOOL, content)?;
    }
    if let Some(ref tags) = args.tags {
        validate::tags(TOOL, tags)?;
    }
    if let Some(ref mt) = args.memory_type {
        validate::memory_type(TOOL, mt)?;
    }
    if let Some(ref refs) = args.external_refs {
        validate::external_refs(TOOL, refs)?;
    }

    // If content changed, re-embed (both dense and sparse).
    let (embedding, sparse_embedding, re_embedded) = if let Some(ref content) = args.content {
        let embed_out = embedder.embed_full(content, TOOL).await?;
        let sparse_json = serde_json::to_value(&embed_out.sparse).map_err(|e| {
            ChittaError::Internal(format!("failed to serialize sparse embedding: {e}"))
        })?;
        (Some(Vector::from(embed_out.dense)), Some(sparse_json), true)
    } else {
        (None, None, false)
    };

    let row = db::update_memory(
        pool,
        &db::MemoryPatch {
            profile: &args.profile,
            id,
            content: args.content.as_deref(),
            embedding: embedding.as_ref(),
            sparse_embedding: sparse_embedding.as_ref(),
            tags: args.tags.as_deref(),
            metadata: args.metadata.as_ref(),
            memory_type: args.memory_type.as_deref(),
            external_refs: args.external_refs.as_ref(),
        },
    )
    .await?
    .ok_or_else(|| ChittaError::NotFound {
        tool: TOOL,
        kind: "memory",
        next_action:
            "Verify the profile and id, or call search_memories to locate the intended memory."
                .to_string(),
    })?;

    Ok(UpdateOutput {
        id: row.id,
        profile: row.profile,
        content: row.content,
        event_time: row.event_time,
        record_time: row.record_time,
        tags: row.tags,
        metadata: row.metadata,
        memory_type: row.memory_type,
        external_refs: row.external_refs,
        re_embedded,
    })
}
