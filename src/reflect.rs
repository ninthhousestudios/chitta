use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;

use crate::db;
use crate::embedding::Embedder;
use crate::error::Result;
use crate::synthesis::{self, Llm, SynthesisResult};

pub async fn reflect_pipeline(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    llm: &(impl Llm + ?Sized),
    profile: &str,
) -> Result<SynthesisResult> {
    let watermark = Utc::now();

    let last_run = db::last_synthesis_run(pool, profile).await?;
    let since = last_run.as_ref().map(|r| r.started_at);

    let rows = db::fetch_raw_since(pool, profile, since).await?;

    if rows.is_empty() {
        eprintln!("reflect: nothing to synthesize for profile '{profile}'");
        return Ok(SynthesisResult {
            clusters_formed: 0,
            clusters_emitted: 0,
            supersessions: 0,
            rows_scanned: 0,
            rows_skipped: 0,
            extraction_errors: 0,
        });
    }

    eprintln!(
        "reflect: {} raw rows since {}",
        rows.len(),
        since.map(|t| t.to_string()).unwrap_or("(all time)".into())
    );

    let result = synthesis::run_synthesis(pool, embedder, llm, profile, &rows, Utc::now()).await?;

    let summary = serde_json::json!({
        "clusters_formed": result.clusters_formed,
        "clusters_emitted": result.clusters_emitted,
        "supersessions": result.supersessions,
        "rows_scanned": result.rows_scanned,
        "rows_skipped": result.rows_skipped,
        "extraction_errors": result.extraction_errors,
    });
    db::insert_reflect_run_with(
        pool,
        profile,
        rows.len() as i32,
        Some(summary),
        watermark,
        Some("synthesis"),
    )
    .await?;

    eprintln!(
        "synthesis: clusters_formed={}, clusters_emitted={}, supersessions={}",
        result.clusters_formed, result.clusters_emitted, result.supersessions
    );

    Ok(result)
}
