//! `get_memory` handler.
//!
//! Profile-scoped fetch by UUID. Returns the full row or a `not_found`
//! error whose `next_action` points at `search_memories`.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "get_memory";

/// Arguments for `get_memory`. `JsonSchema` is derived so rmcp exposes the
/// same shape callers use on the wire.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetArgs {
    /// Profile scope.
    pub profile: String,
    /// Memory UUID.
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct GetOutput {
    pub id: Uuid,
    pub profile: String,
    pub content: String,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_refs: Option<serde_json::Value>,
}

#[tracing::instrument(
    name = "tool.get_memory",
    skip(pool, args),
    fields(profile = %args.profile, id = %args.id),
)]
pub async fn handle(pool: &PgPool, args: GetArgs) -> Result<GetOutput> {
    validate::profile(TOOL, &args.profile)?;
    let id = validate::parse_uuid(TOOL, "id", &args.id)?;

    let row = db::get_memory_by_id(pool, &args.profile, id).await?.ok_or_else(|| {
        ChittaError::NotFound {
            tool: TOOL,
            kind: "memory",
            next_action:
                "Verify the profile and id, or call search_memories to locate the intended memory."
                    .to_string(),
        }
    })?;

    Ok(GetOutput {
        id: row.id,
        profile: row.profile,
        content: row.content,
        event_time: row.event_time,
        record_time: row.record_time,
        tags: row.tags,
        source: row.source,
        metadata: row.metadata,
        memory_type: row.memory_type,
        external_refs: row.external_refs,
    })
}
