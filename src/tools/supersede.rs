use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "supersede_memory";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SupersedeArgs {
    /// Target profile namespace.
    pub profile: String,
    /// UUID of the memory being superseded.
    pub old_id: String,
    /// UUID of the replacement memory (must already exist).
    pub new_id: String,
    /// Why the old memory is being superseded.
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct SupersedeOutput {
    pub old_id: Uuid,
    pub new_id: Uuid,
    pub derivation_id: Uuid,
}

#[tracing::instrument(
    name = "tool.supersede_memory",
    skip(pool, args),
    fields(profile = %args.profile),
)]
pub async fn handle(pool: &PgPool, args: SupersedeArgs) -> Result<SupersedeOutput> {
    validate::profile(TOOL, &args.profile)?;

    let old_id = Uuid::parse_str(&args.old_id).map_err(|_| ChittaError::InvalidArgument {
        tool: TOOL,
        argument: "old_id".into(),
        constraint: "must be a valid UUID".into(),
        received: Some(serde_json::json!(args.old_id)),
        next_action: "Pass a valid UUID for old_id.".into(),
    })?;

    let new_id = Uuid::parse_str(&args.new_id).map_err(|_| ChittaError::InvalidArgument {
        tool: TOOL,
        argument: "new_id".into(),
        constraint: "must be a valid UUID".into(),
        received: Some(serde_json::json!(args.new_id)),
        next_action: "Pass a valid UUID for new_id.".into(),
    })?;

    if args.reason.trim().is_empty() {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "reason".into(),
            constraint: "must be non-empty".into(),
            received: None,
            next_action: "Provide a reason for the supersession.".into(),
        });
    }

    let old_row =
        db::get_memory_by_id(pool, &args.profile, old_id)
            .await?
            .ok_or_else(|| ChittaError::NotFound {
                tool: TOOL,
                kind: "memory",
                next_action: format!(
                    "old_id {old_id} not found in profile '{}'. Check the id and profile.",
                    args.profile
                ),
            })?;

    if old_row.superseded_by.is_some() {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "old_id".into(),
            constraint: "must not already be superseded".into(),
            received: Some(serde_json::json!(args.old_id)),
            next_action: format!(
                "Memory {old_id} is already superseded by {}.",
                old_row.superseded_by.unwrap()
            ),
        });
    }

    db::get_memory_by_id(pool, &args.profile, new_id)
        .await?
        .ok_or_else(|| ChittaError::NotFound {
            tool: TOOL,
            kind: "memory",
            next_action: format!(
                "new_id {new_id} not found in profile '{}'. \
                 Store the new memory first, then call supersede_memory.",
                args.profile
            ),
        })?;

    let derivation = db::supersede_memory(pool, old_id, new_id).await?;

    Ok(SupersedeOutput {
        old_id,
        new_id,
        derivation_id: derivation.id,
    })
}
