use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::Result;
use crate::tools::validate;

const TOOL: &str = "reflect_summary";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReflectSummaryArgs {
    /// Profile to reflect on.
    pub profile: String,
}

#[derive(Debug, Serialize)]
pub struct ReflectSummaryOutput {
    pub profile: String,
    pub since: Option<DateTime<Utc>>,
    pub last_run_id: Option<Uuid>,
    pub counts: BTreeMap<String, usize>,
    pub total: usize,
    pub date_range: Option<DateRange>,
    pub distinct_domains: Vec<String>,
    pub distinct_skills: Vec<String>,
    pub distinct_projects: Vec<String>,
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
    name = "tool.reflect_summary",
    skip(pool, args),
    fields(profile = %args.profile),
)]
pub async fn handle(pool: &PgPool, args: ReflectSummaryArgs) -> Result<ReflectSummaryOutput> {
    validate::profile(TOOL, &args.profile)?;

    let last_run = db::last_reflect_run(pool, &args.profile).await?;
    let since = last_run.as_ref().and_then(|r| r.completed_at);
    let last_run_id = last_run.as_ref().map(|r| r.id);

    let rows = db::fetch_raw_since(pool, &args.profile, since).await?;

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut domains: BTreeSet<String> = BTreeSet::new();
    let mut skills: BTreeSet<String> = BTreeSet::new();
    let mut projects: BTreeSet<String> = BTreeSet::new();
    let mut disagree_flagged: Vec<DisagreeFlagged> = Vec::new();
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut latest: Option<DateTime<Utc>> = None;

    for row in &rows {
        *counts.entry(row.memory_type.clone()).or_default() += 1;

        for d in &row.applies_to_domains {
            domains.insert(d.clone());
        }
        for s in &row.applies_to_skills {
            skills.insert(s.clone());
        }
        for p in &row.applies_to_projects {
            projects.insert(p.clone());
        }

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

        if row.tags.iter().any(|t| t == "feedback")
            && row.tags.iter().any(|t| t == "disagree")
        {
            let snippet: String = row.content.chars().take(120).collect();
            disagree_flagged.push(DisagreeFlagged {
                memory_id: row.id,
                content_snippet: snippet,
                record_time: row.record_time,
            });
        }
    }

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

    let run = db::insert_reflect_run(
        pool,
        &args.profile,
        total as i32,
        Some(summary_json),
    )
    .await?;

    Ok(ReflectSummaryOutput {
        profile: args.profile,
        since,
        last_run_id,
        counts,
        total,
        date_range,
        distinct_domains: domains.into_iter().collect(),
        distinct_skills: skills.into_iter().collect(),
        distinct_projects: projects.into_iter().collect(),
        disagree_flagged,
        run_id: run.id,
    })
}
