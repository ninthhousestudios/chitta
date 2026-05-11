use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::Result;
use crate::facets::Facets;
use crate::tools::validate;

const TOOL: &str = "reflect_status";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReflectStatusArgs {
    /// Profile to reflect on.
    pub profile: String,
}

#[derive(Debug, Serialize)]
pub struct ReflectStatusOutput {
    pub profile: String,
    pub since: Option<DateTime<Utc>>,
    pub last_run_id: Option<Uuid>,
    pub counts: BTreeMap<String, usize>,
    pub total: usize,
    pub date_range: Option<DateRange>,
    pub distinct_domains: Vec<String>,
    pub distinct_skills: Vec<String>,
    pub distinct_projects: Vec<String>,
    pub distinct_situations: Vec<String>,
    pub disagree_flagged: Vec<DisagreeFlagged>,
    pub run_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct DateRange {
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct DisagreeFlagged {
    pub memory_id: Uuid,
    pub content_snippet: String,
    pub record_time: DateTime<Utc>,
}

#[tracing::instrument(
    name = "tool.reflect_status",
    skip(pool, args),
    fields(profile = %args.profile),
)]
pub async fn handle(pool: &PgPool, args: ReflectStatusArgs) -> Result<ReflectStatusOutput> {
    validate::profile(TOOL, &args.profile)?;

    let last_run = db::last_reflect_run(pool, &args.profile).await?;
    let since = last_run.as_ref().and_then(|r| r.completed_at);
    let last_run_id = last_run.as_ref().map(|r| r.id);

    let rows = db::fetch_raw_since(pool, &args.profile, since).await?;

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut disagree_flagged: Vec<DisagreeFlagged> = Vec::new();
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;

    for row in &rows {
        *counts.entry(row.memory_type.clone()).or_default() += 1;

        match earliest {
            None => earliest = Some(row.record_time),
            Some(e) if row.record_time < e => earliest = Some(row.record_time),
            _ => {}
        }
        match latest {
            None => latest = Some(row.record_time),
            Some(l) if row.record_time > l => latest = Some(row.record_time),
            _ => {}
        }

        if row.tags.iter().any(|t| t == "feedback") && row.tags.iter().any(|t| t == "disagree") {
            let snippet: String = row.content.chars().take(120).collect();
            disagree_flagged.push(DisagreeFlagged {
                memory_id: row.id,
                content_snippet: snippet,
                record_time: row.record_time,
            });
        }
    }

    let facet_summary = Facets::distinct_union(&rows);

    let total = rows.len();
    let date_range = earliest.zip(latest).map(|(e, l)| DateRange {
        earliest: e,
        latest: l,
    });

    let summary_json = serde_json::json!({
        "counts": counts,
        "total": total,
        "disagree_flagged_count": disagree_flagged.len(),
    });

    let run = db::insert_reflect_run_with(
        pool,
        &args.profile,
        total as i32,
        Some(summary_json),
        chrono::Utc::now(),
        Some("status"),
    )
    .await?;

    Ok(ReflectStatusOutput {
        profile: args.profile,
        since,
        last_run_id,
        counts,
        total,
        date_range,
        distinct_domains: facet_summary.domains,
        distinct_skills: facet_summary.skills,
        distinct_projects: facet_summary.projects,
        distinct_situations: facet_summary.situations,
        disagree_flagged,
        run_id: run.id,
    })
}
