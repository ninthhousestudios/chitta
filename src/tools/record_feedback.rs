use std::sync::Arc;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::consolidated::{is_active, is_consolidated};
use crate::db;
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::tools::validate;

const TOOL: &str = "record_feedback";

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind {
    Agree,
    Disagree,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct RecordFeedbackArgs {
    /// Target profile namespace.
    pub profile: String,
    /// UUID of the consolidated memory to give feedback on.
    pub memory_id: String,
    /// "agree" or "disagree".
    pub kind: FeedbackKind,
    /// Correction text. Only valid when kind=disagree. /reflect picks this
    /// up later as contradicting evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RecordFeedbackOutput {
    pub memory_id: Uuid,
    pub new_confidence: f32,
    pub kind: FeedbackKind,
    pub feedback_row_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_row_id: Option<Uuid>,
}

const AGREE_DELTA: f32 = 0.05;
const DISAGREE_DELTA: f32 = 0.10;

#[tracing::instrument(
    name = "tool.record_feedback",
    skip(pool, embedder, args),
    fields(profile = %args.profile, memory_id = %args.memory_id),
)]
pub async fn handle(
    pool: &PgPool,
    embedder: Arc<Embedder>,
    args: RecordFeedbackArgs,
) -> Result<RecordFeedbackOutput> {
    validate::profile(TOOL, &args.profile)?;
    let memory_id = validate::parse_uuid(TOOL, "memory_id", &args.memory_id)?;

    if matches!(args.kind, FeedbackKind::Agree) && args.correction.is_some() {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "correction".to_string(),
            constraint: "correction is only valid when kind=disagree".to_string(),
            received: None,
            next_action: "Remove the correction field, or change kind to disagree.".to_string(),
        });
    }

    let row = db::get_memory_by_id(pool, &args.profile, memory_id)
        .await?
        .ok_or_else(|| ChittaError::NotFound {
            tool: TOOL,
            kind: "memory",
            next_action: "Verify the profile and memory_id. Use search_memories to locate the intended memory.".to_string(),
        })?;

    if !is_consolidated(&row.memory_type) {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "memory_id".to_string(),
            constraint: format!(
                "target must be a consolidated type (trait, value, pattern, preference, mental_model), got '{}'",
                row.memory_type
            ),
            received: Some(serde_json::json!(row.memory_type)),
            next_action: "Feedback can only be recorded on consolidated memories returned by get_profile.".to_string(),
        });
    }

    if !is_active(&row) {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "memory_id".to_string(),
            constraint: "target memory is superseded or invalidated".to_string(),
            received: None,
            next_action: "Use get_profile to find the current active version of this memory.".to_string(),
        });
    }

    let now = Utc::now();
    let old_confidence = row.confidence.unwrap_or(0.5);
    let new_confidence = match args.kind {
        FeedbackKind::Agree => (old_confidence + AGREE_DELTA).min(1.0),
        FeedbackKind::Disagree => (old_confidence - DISAGREE_DELTA).max(0.0),
    };
    let new_reinforcement_count = match args.kind {
        FeedbackKind::Agree => row.reinforcement_count + 1,
        FeedbackKind::Disagree => row.reinforcement_count,
    };

    db::apply_feedback(pool, &args.profile, memory_id, new_confidence, new_reinforcement_count, now).await?;

    let feedback_tag = match args.kind {
        FeedbackKind::Agree => "agree",
        FeedbackKind::Disagree => "disagree",
    };
    let feedback_content = match args.kind {
        FeedbackKind::Agree => format!("Feedback: agree with memory {memory_id}"),
        FeedbackKind::Disagree => format!("Feedback: disagree with memory {memory_id}"),
    };
    let feedback_refs = serde_json::json!([
        {"kind": "memory", "ref": memory_id.to_string()}
    ]);

    let feedback_row = db::insert_or_fetch_memory(
        pool,
        &db::MemoryRow {
            id: Uuid::now_v7(),
            profile: args.profile.clone(),
            content: feedback_content,
            embedding: None,
            sparse_embedding: None,
            event_time: now,
            record_time: now,
            idempotency_key: format!("feedback-{}-{}-{}", memory_id, feedback_tag, now.timestamp_millis()),
            source: Some("record_feedback".to_string()),
            memory_type: "observation".to_string(),
            tags: vec!["feedback".to_string(), feedback_tag.to_string()],
            external_refs: Some(feedback_refs),
            metadata: None,
            facets: row.facets.clone(),
            superseded_by: None,
            confidence: None,
            reinforcement_count: 0,
            last_reinforced_at: None,
            invalidated_at: None,
        },
    )
    .await?;

    let correction_row_id = if let Some(ref correction_text) = args.correction {
        validate::content_non_empty(TOOL, correction_text)?;
        validate::content_byte_length(TOOL, correction_text)?;

        let correction_refs = serde_json::json!([
            {"kind": "memory", "ref": memory_id.to_string()}
        ]);

        let embed_out = embedder.embed_full(correction_text, TOOL).await?;
        let sparse_json = serde_json::to_value(&embed_out.sparse).map_err(|e| {
            ChittaError::Internal(format!("failed to serialize sparse embedding: {e}"))
        })?;

        let correction_row = db::insert_or_fetch_memory(
            pool,
            &db::MemoryRow {
                id: Uuid::now_v7(),
                profile: args.profile.clone(),
                content: correction_text.clone(),
                embedding: Some(pgvector::Vector::from(embed_out.dense)),
                sparse_embedding: Some(sparse_json),
                event_time: now,
                record_time: now,
                idempotency_key: format!("correction-{}-{}", memory_id, now.timestamp_millis()),
                source: Some("record_feedback".to_string()),
                memory_type: "observation".to_string(),
                tags: vec![
                    "correction".to_string(),
                    format!("contradicts:{memory_id}"),
                ],
                external_refs: Some(correction_refs),
                metadata: None,
                facets: row.facets.clone(),
                superseded_by: None,
                confidence: None,
                reinforcement_count: 0,
                last_reinforced_at: None,
                invalidated_at: None,
            },
        )
        .await?;
        Some(correction_row.0.id)
    } else {
        None
    };

    Ok(RecordFeedbackOutput {
        memory_id,
        new_confidence,
        kind: args.kind,
        feedback_row_id: feedback_row.0.id,
        correction_row_id,
    })
}
