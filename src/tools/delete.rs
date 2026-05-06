//! `delete_memory` handler.
//!
//! Soft-delete a memory by setting `invalidated_at`. The row remains in the
//! database but is excluded from all read paths.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "delete_memory";

/// Arguments for `delete_memory`. `JsonSchema` is derived so rmcp exposes the
/// same shape on the wire.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct DeleteArgs {
    /// Profile scope.
    pub profile: String,
    /// Memory UUID to delete.
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteOutput {
    pub id: Uuid,
    pub deleted: bool,
}

#[tracing::instrument(
    name = "tool.delete_memory",
    skip(pool, args),
    fields(profile = %args.profile, id = %args.id),
)]
pub async fn handle(pool: &PgPool, args: DeleteArgs) -> Result<DeleteOutput> {
    validate::profile(TOOL, &args.profile)?;
    let id = validate::parse_uuid(TOOL, "id", &args.id)?;

    let deleted = db::delete_memory(pool, &args.profile, id).await?;

    if !deleted {
        return Err(ChittaError::NotFound {
            tool: TOOL,
            kind: "memory",
            next_action:
                "Verify the profile and id, or call search_memories to locate the intended memory."
                    .to_string(),
        });
    }

    Ok(DeleteOutput { id, deleted: true })
}
