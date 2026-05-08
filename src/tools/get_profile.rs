use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::error::Result;
use crate::scoring;
use crate::tools::validate;

const TOOL: &str = "get_profile";
const PROFILE_LIMIT: usize = 30;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetProfileArgs {
    /// Profile to load the working model for.
    pub profile: String,
}

#[derive(Debug, Serialize)]
pub struct ProfileEntry {
    pub id: Uuid,
    pub content: String,
    pub memory_type: String,
    pub confidence: f32,
    pub effective_score: f32,
    pub event_time: DateTime<Utc>,
    pub record_time: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub applies_to_domains: Vec<String>,
    pub applies_to_skills: Vec<String>,
    pub applies_to_projects: Vec<String>,
    pub applies_to_situations: Vec<String>,
    pub reinforcement_count: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reinforced_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct GetProfileOutput {
    pub profile: String,
    pub entries: Vec<ProfileEntry>,
    pub total_candidates: usize,
    pub truncated: bool,
}

#[tracing::instrument(
    name = "tool.get_profile",
    skip(pool, args),
    fields(profile = %args.profile),
)]
pub async fn handle(pool: &PgPool, args: GetProfileArgs) -> Result<GetProfileOutput> {
    validate::profile(TOOL, &args.profile)?;

    let now = Utc::now();
    let candidates = db::fetch_profile_candidates(pool, &args.profile).await?;
    let total_candidates = candidates.len();

    let mut scored: Vec<(f32, _)> = candidates
        .into_iter()
        .map(|row| {
            let confidence = row.confidence.unwrap_or(0.0);
            let es = scoring::effective_score(
                confidence,
                row.last_reinforced_at,
                row.record_time,
                now,
            );
            (es, row)
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(PROFILE_LIMIT);

    let truncated = total_candidates > PROFILE_LIMIT;
    let entries = scored
        .into_iter()
        .map(|(es, row)| ProfileEntry {
            id: row.id,
            content: row.content,
            memory_type: row.memory_type,
            confidence: row.confidence.unwrap_or(0.0),
            effective_score: es,
            event_time: row.event_time,
            record_time: row.record_time,
            tags: row.tags,
            metadata: row.metadata,
            applies_to_domains: row.applies_to_domains,
            applies_to_skills: row.applies_to_skills,
            applies_to_projects: row.applies_to_projects,
            applies_to_situations: row.applies_to_situations,
            reinforcement_count: row.reinforcement_count,
            last_reinforced_at: row.last_reinforced_at,
        })
        .collect();

    Ok(GetProfileOutput {
        profile: args.profile,
        entries,
        total_candidates,
        truncated,
    })
}
