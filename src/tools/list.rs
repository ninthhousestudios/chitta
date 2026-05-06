//! `list_recent_memories` handler.
//!
//! Lists memories ordered by `record_time DESC` with optional tag filter.
//! Returns 200-char snippets; full content via `get_memory`.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::{ChittaError, Result};
use crate::tools::search::prefix_chars;
use crate::tools::validate;

const TOOL: &str = "list_recent_memories";

/// Default `limit` when the caller does not set one.
const DEFAULT_LIMIT: i64 = 20;

/// Maximum `limit`.
const MAX_LIMIT: i64 = 200;

/// Snippet length in chars (not bytes). Verbatim prefix; no ellipsis.
const SNIPPET_CHARS: usize = 200;

/// Arguments for `list_recent_memories`. `JsonSchema` is derived so rmcp
/// exposes the same shape on the wire.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListArgs {
    /// Profile scope.
    pub profile: String,
    /// Max number of results. Default 20; hard cap 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// OR-match: a memory matches if it has any of these tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Filter by memory type(s). OR-match: returns memories matching any listed type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_types: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct ListItem {
    pub id: Uuid,
    pub snippet: String,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub memory_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_refs: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ListOutput {
    pub memories: Vec<ListItem>,
    pub total_in_profile: i64,
}

#[tracing::instrument(
    name = "tool.list_recent_memories",
    skip(pool, args),
    fields(profile = %args.profile, limit = ?args.limit),
)]
pub async fn handle(pool: &PgPool, args: ListArgs) -> Result<ListOutput> {
    validate::profile(TOOL, &args.profile)?;

    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "limit".to_string(),
            constraint: format!("integer in [1, {MAX_LIMIT}]"),
            received: Some(serde_json::json!(limit)),
            next_action: format!(
                "Pass limit between 1 and {MAX_LIMIT} (default is {DEFAULT_LIMIT})."
            ),
        });
    }

    let tags = args.tags.unwrap_or_default();
    validate::tags(TOOL, &tags)?;
    let memory_types = args.memory_types.unwrap_or_default();
    validate::memory_types(TOOL, &memory_types)?;

    let (rows, total_in_profile) =
        db::list_recent_with_count(pool, &args.profile, limit, &tags, &memory_types).await?;

    let memories = rows
        .into_iter()
        .map(|row| ListItem {
            id: row.id,
            snippet: prefix_chars(&row.content, SNIPPET_CHARS),
            event_time: row.event_time,
            record_time: row.record_time,
            tags: row.tags,
            source: row.source,
            memory_type: row.memory_type,
            external_refs: row.external_refs,
        })
        .collect();

    Ok(ListOutput {
        memories,
        total_in_profile,
    })
}
