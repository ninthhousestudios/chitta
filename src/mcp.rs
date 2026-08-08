//! rmcp server: three tools, stdio transport.
//!
//! Tool handlers live in [`crate::tools`]; this module wires them to rmcp's
//! `ToolRouter` and maps [`ChittaError`](crate::error::ChittaError) to
//! JSON-RPC errors whose `data` field carries the Principle 8 contract
//! (`tool`, `constraint`, `next_action`).

use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use sqlx::PgPool;

use crate::config::SearchConfig;
use crate::embedding::LazyEmbedder;
use crate::error::ChittaError;
use crate::tools;

/// Shared server state. Cheap to clone (Arc + PgPool both clone-cheap).
#[derive(Clone)]
pub struct ChittaServer {
    pool: PgPool,
    embedder: Arc<LazyEmbedder>,
    query_log_enabled: bool,
    search_cfg: SearchConfig,
    tool_router: ToolRouter<Self>,
}

impl ChittaServer {
    pub fn new(
        pool: PgPool,
        embedder: Arc<LazyEmbedder>,
        query_log_enabled: bool,
        search_cfg: SearchConfig,
    ) -> Self {
        Self {
            pool,
            embedder,
            query_log_enabled,
            search_cfg,
            tool_router: Self::tool_router(),
        }
    }
}

// ---- Tool handlers ---------------------------------------------------
//
// The `tools::*Args` types carry `#[derive(JsonSchema)]` directly, so rmcp
// publishes them on the wire from a single source of truth — no mirror
// structs, no field-drift risk.

#[tool_router(router = tool_router)]
impl ChittaServer {
    /// Store a new memory. Idempotent on (profile, idempotency_key).
    #[tool(
        description = "Store a new memory. Idempotent on (profile, idempotency_key): \
                          resubmitting the same key returns the prior row with \
                          idempotent_replay=true."
    )]
    pub async fn store_memory(
        &self,
        Parameters(args): Parameters<tools::StoreArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::store::handle(&self.pool, Arc::clone(&self.embedder), args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Fetch a memory by id.
    #[tool(description = "Fetch a memory by profile + id. Returns the full row. \
                          Errors with not_found if the id is unknown in that profile.")]
    pub async fn get_memory(
        &self,
        Parameters(args): Parameters<tools::GetArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::get::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Semantic similarity search. Returns envelope with snippets.
    #[tool(
        description = "Semantic search. Returns an envelope with 200-char snippets, \
                          similarity scores, and honest truncated/total_available. \
                          Call get_memory(id) to read full content."
    )]
    pub async fn search_memories(
        &self,
        Parameters(args): Parameters<tools::SearchArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::search::handle(
            &self.pool,
            Arc::clone(&self.embedder),
            self.query_log_enabled,
            &self.search_cfg,
            args,
        )
        .await
        .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Update a memory's content and/or tags.
    #[tool(
        description = "Update a memory's content and/or tags by profile + id. \
                          At least one of content or tags must be provided. \
                          If content changes, the embedding is recomputed. \
                          record_time is never updated."
    )]
    pub async fn update_memory(
        &self,
        Parameters(args): Parameters<tools::UpdateArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::update::handle(&self.pool, Arc::clone(&self.embedder), args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Delete a memory by profile + id.
    #[tool(description = "Hard-delete a memory by profile + id. \
                          Returns the deleted id. Errors with not_found if the id \
                          is unknown in that profile.")]
    pub async fn delete_memory(
        &self,
        Parameters(args): Parameters<tools::DeleteArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::delete::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// List recent memories ordered by record_time DESC.
    #[tool(description = "List recent memories ordered by record_time DESC with \
                          200-char snippets. Optional tag filter (OR match). \
                          Default limit 20, max 200. Call get_memory(id) for full content.")]
    pub async fn list_recent_memories(
        &self,
        Parameters(args): Parameters<tools::ListArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::list::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Supersede a memory: mark old as superseded by new, write derivation row atomically.
    #[tool(
        description = "Supersede a memory. Sets superseded_by on old_id, writes a \
                          derivation row linking new_id → old_id with type 'supersedes', \
                          in one transaction. Both memories must exist in the profile. \
                          Superseded memories are excluded from default search."
    )]
    pub async fn supersede_memory(
        &self,
        Parameters(args): Parameters<tools::SupersedeArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::supersede::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Load the always-on working-model profile for a subject.
    #[tool(
        description = "Load the always-on working-model profile. Returns up to 30 \
                          consolidated memories (traits, values, preferences, patterns, \
                          mental models) ordered by effective_score — a decay-weighted \
                          combination of confidence and recency. Call at session start \
                          with profile='josh' to ground the conversation."
    )]
    pub async fn get_profile(
        &self,
        Parameters(args): Parameters<tools::GetProfileArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::get_profile::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Reflect status: scan raw rows since last /reflect run and return a summary.
    #[tool(
        description = "Reflect status. Reads raw rows (observation, episode, decision) \
                          since the last /reflect run and returns counts by type, date range, \
                          distinct facet values, and disagree-flagged memory IDs. Writes a \
                          run-marker so the next invocation only reads newer rows. Does NOT \
                          write consolidated rows — that is the full /reflect pipeline."
    )]
    pub async fn reflect_status(
        &self,
        Parameters(args): Parameters<tools::ReflectStatusArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::reflect_status::handle(&self.pool, args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Record feedback (agree/disagree) on a consolidated memory.
    #[tool(
        description = "Record feedback on a consolidated memory (trait, value, \
                          pattern, preference, mental_model). kind=agree bumps \
                          confidence +0.05 (cap 1.0) and increments reinforcement_count. \
                          kind=disagree drops confidence −0.10 (floor 0.0). Optional \
                          correction text (disagree only) writes a separate observation \
                          tagged contradicts:<memory_id> for /reflect to pick up."
    )]
    pub async fn record_feedback(
        &self,
        Parameters(args): Parameters<tools::RecordFeedbackArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::record_feedback::handle(&self.pool, Arc::clone(&self.embedder), args)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }

    /// Health check — verifies DB connectivity and embedder responsiveness.
    #[tool(
        description = "Health check. Verifies DB connectivity and ONNX embedder \
                          responsiveness. Returns status (ok/degraded), component \
                          health flags, pool size, and server version. Call at \
                          session start to confirm the server is operational."
    )]
    pub async fn health_check(
        &self,
        Parameters(_args): Parameters<tools::HealthArgs>,
    ) -> Result<String, ErrorData> {
        let out = tools::health::handle(&self.pool, Arc::clone(&self.embedder), &self.search_cfg)
            .await
            .map_err(chitta_to_rmcp)?;
        serde_json::to_string_pretty(&out).map_err(json_to_rmcp)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChittaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "chitta v0.3.0 — working model of Josh. \
                 Eleven tools: store_memory, get_memory, search_memories, \
                 update_memory, delete_memory, list_recent_memories, \
                 supersede_memory, get_profile, record_feedback, reflect_status, \
                 health_check. \
                 Profiles isolate namespaces; idempotency_key dedupes writes; \
                 bi-temporal (event_time + record_time); verbatim storage.",
        )
    }
}

// ---- Error translation -----------------------------------------------

/// Translate a [`ChittaError`] into rmcp's `ErrorData` shape. `pub` so the
/// contract test suite can exercise every variant through the actual mapper
/// — if this fn ever drops a field or misroutes a code, `tests/contract.rs`
/// catches it.
pub fn chitta_to_rmcp(e: ChittaError) -> ErrorData {
    let code = e.code();
    let message = e.message();
    let data = serde_json::to_value(e.data()).ok();
    // rmcp exposes named constructors for specific codes; fall back to `new`
    // so our mapping stays authoritative.
    if code == crate::error::codes::INVALID_PARAMS {
        ErrorData::invalid_params(message, data)
    } else {
        ErrorData::internal_error(message, data)
    }
}

pub fn json_to_rmcp(e: serde_json::Error) -> ErrorData {
    ErrorData::internal_error(
        format!("failed to serialize response: {e}"),
        Some(serde_json::json!({
            "tool": "server",
            "constraint": "response serializes to JSON",
            "next_action": "Report this as a bug; include server logs.",
        })),
    )
}
