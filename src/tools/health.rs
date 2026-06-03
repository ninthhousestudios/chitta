//! `health_check` handler.
//!
//! Verifies DB connectivity, ONNX embedder responsiveness, and reports
//! pool status. Designed for agent startup probes (e.g. CLAUDE.md's
//! `mcp__chittars__health_check` call).

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::config::SearchConfig;
use crate::embedding::LazyEmbedder;
use crate::error::Result;

const TOOL: &str = "health_check";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HealthArgs {}

#[derive(Debug, Serialize)]
pub struct HealthOutput {
    pub status: &'static str,
    pub retrieval_legs: Vec<&'static str>,
    pub db_connected: bool,
    pub embedder_ok: bool,
    pub model_loaded: bool,
    pub embedder_pool_size: usize,
    pub version: &'static str,
}

#[tracing::instrument(name = "tool.health_check", skip(pool, embedder, search_cfg))]
pub async fn handle(
    pool: &PgPool,
    embedder: Arc<LazyEmbedder>,
    search_cfg: &SearchConfig,
) -> Result<HealthOutput> {
    let db_connected = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .is_ok();

    let model_loaded = embedder.is_loaded();
    let embedder_ok = if model_loaded {
        embedder.embed("health check probe", TOOL).await.is_ok()
    } else {
        true
    };

    let all_ok = db_connected && embedder_ok;

    let mut legs = vec!["dense"];
    if search_cfg.rrf_fts {
        legs.push("fts");
    }
    if search_cfg.rrf_sparse {
        legs.push("sparse");
    }

    Ok(HealthOutput {
        status: if all_ok { "ok" } else { "degraded" },
        retrieval_legs: legs,
        db_connected,
        embedder_ok,
        model_loaded,
        embedder_pool_size: embedder.pool_size(),
        version: env!("CARGO_PKG_VERSION"),
    })
}
