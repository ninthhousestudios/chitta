// BUG: watermark-advance-on-empty-emit
//
// reflect_pipeline always writes a reflect_runs row at the end (line ~85),
// advancing the watermark past all rows it scanned. If clusters form but
// none pass the threshold (e.g. min_cluster_size too high), those source
// rows are never re-examined — the next run's `fetch_raw_since` starts
// after the watermark, so they're silently orphaned.
//
// The fix would be: only advance the watermark past rows that were actually
// emitted (or explicitly discarded by the user). But the autonomous pipeline
// is disabled in favor of the interactive /reflect skill, so this is left
// as a known issue for now.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use sqlx::PgPool;

use crate::config::chitta_home;
use crate::db;
use crate::embedding::Embedder;
use crate::error::Result;
use crate::synthesis::{self, extract_candidates, ExtractionStats, Llm, SynthesisResult};

fn checkpoint_path(profile: &str) -> PathBuf {
    chitta_home().join(format!("reflect-checkpoint-{profile}.json"))
}

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

    let cp = checkpoint_path(profile);
    let extraction = if cp.exists() {
        match std::fs::read_to_string(&cp).and_then(|s| {
            serde_json::from_str::<ExtractionStats>(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(cached) => {
                eprintln!(
                    "resuming from checkpoint: {} cached candidates ({} rows)",
                    cached.candidates.len(),
                    cached.rows_scanned,
                );
                cached
            }
            Err(e) => {
                eprintln!("checkpoint unreadable ({e}), re-extracting");
                run_extraction_with_checkpoint(llm, &rows, &cp).await?
            }
        }
    } else {
        run_extraction_with_checkpoint(llm, &rows, &cp).await?
    };

    let result =
        synthesis::run_synthesis_from(pool, embedder, llm, profile, &rows, extraction, Utc::now())
            .await?;

    let _ = std::fs::remove_file(&cp);

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

async fn run_extraction_with_checkpoint(
    llm: &(impl Llm + ?Sized),
    rows: &[db::MemoryRow],
    checkpoint: &PathBuf,
) -> Result<ExtractionStats> {
    let extraction = extract_candidates(llm, rows).await?;
    eprintln!(
        "extraction done: {} candidates from {} rows ({} skipped, {} errors)",
        extraction.candidates.len(),
        extraction.rows_scanned,
        extraction.rows_skipped,
        extraction.extraction_errors,
    );
    if let Ok(json) = serde_json::to_string(&extraction) {
        let _ = std::fs::write(checkpoint, json);
        eprintln!("checkpoint saved to {}", checkpoint.display());
    }
    Ok(extraction)
}
