use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::consolidated::{CONSOLIDATED_TYPES, is_active, is_consolidated};
use crate::db;
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::facets::Facets;
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

struct PreparedCorrection {
    embedding: Vector,
    sparse_json: serde_json::Value,
    content: String,
}

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
    // ── Phase 1: validate everything before any DB work ──────────
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

    // ── Phase 2: embed correction (expensive, before tx) ─────────
    let prepared_correction = if let Some(ref correction_text) = args.correction {
        validate::content_non_empty(TOOL, correction_text)?;
        validate::content_byte_length(TOOL, correction_text)?;

        let embed_out = embedder.embed_full(correction_text, TOOL).await?;
        let sparse_json = serde_json::to_value(&embed_out.sparse).map_err(|e| {
            ChittaError::Internal(format!("failed to serialize sparse embedding: {e}"))
        })?;

        Some(PreparedCorrection {
            embedding: Vector::from(embed_out.dense),
            sparse_json,
            content: correction_text.clone(),
        })
    } else {
        None
    };

    // ── Phase 3: single transaction for all DB mutations ─────────
    let now = Utc::now();
    let feedback_tag = match args.kind {
        FeedbackKind::Agree => "agree",
        FeedbackKind::Disagree => "disagree",
    };

    let result = execute_feedback_tx(
        pool,
        &args.profile,
        memory_id,
        &args.kind,
        feedback_tag,
        now,
        prepared_correction.as_ref(),
    )
    .await?;

    Ok(RecordFeedbackOutput {
        memory_id,
        new_confidence: result.new_confidence,
        kind: args.kind,
        feedback_row_id: result.feedback_row_id,
        correction_row_id: result.correction_row_id,
    })
}

struct TxResult {
    new_confidence: f32,
    feedback_row_id: Uuid,
    correction_row_id: Option<Uuid>,
}

async fn execute_feedback_tx(
    pool: &PgPool,
    profile: &str,
    memory_id: Uuid,
    kind: &FeedbackKind,
    feedback_tag: &str,
    now: DateTime<Utc>,
    prepared_correction: Option<&PreparedCorrection>,
) -> Result<TxResult> {
    let mut tx = pool.begin().await?;

    // Lock and fetch target row
    let row = sqlx::query_as::<_, db::MemoryRow>(
        r#"
        SELECT id, profile, content, embedding, sparse_embedding,
               event_time, record_time, idempotency_key, source,
               memory_type, tags, external_refs, metadata,
               applies_to_domains, applies_to_skills,
               applies_to_projects, applies_to_situations,
               superseded_by, confidence, reinforcement_count,
               last_reinforced_at, invalidated_at
        FROM memories
        WHERE profile = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(profile)
    .bind(memory_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ChittaError::NotFound {
        tool: TOOL,
        kind: "memory",
        next_action:
            "Verify the profile and memory_id. Use search_memories to locate the intended memory."
                .to_string(),
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
            next_action:
                "Feedback can only be recorded on consolidated memories returned by get_profile."
                    .to_string(),
        });
    }

    if !is_active(&row) {
        return Err(ChittaError::InvalidArgument {
            tool: TOOL,
            argument: "memory_id".to_string(),
            constraint: "target memory is superseded or invalidated".to_string(),
            received: None,
            next_action: "Use get_profile to find the current active version of this memory."
                .to_string(),
        });
    }

    // Atomic confidence update in SQL — no read-then-write race
    let (confidence_expr, bump_reinforcement) = match kind {
        FeedbackKind::Agree => ("LEAST(COALESCE(confidence, 0.5) + $3, 1.0)", true),
        FeedbackKind::Disagree => ("GREATEST(COALESCE(confidence, 0.5) - $3, 0.0)", false),
    };
    let delta = match kind {
        FeedbackKind::Agree => AGREE_DELTA,
        FeedbackKind::Disagree => DISAGREE_DELTA,
    };

    let reinforcement_expr = if bump_reinforcement {
        "reinforcement_count + 1"
    } else {
        "reinforcement_count"
    };

    let reinforced_at_expr = if bump_reinforcement {
        "$4"
    } else {
        "last_reinforced_at"
    };

    let query = format!(
        r#"
        UPDATE memories
        SET confidence          = {confidence_expr},
            reinforcement_count = {reinforcement_expr},
            last_reinforced_at  = {reinforced_at_expr}
        WHERE profile = $1 AND id = $2
          AND invalidated_at IS NULL
          AND superseded_by IS NULL
          AND memory_type = ANY($5)
        RETURNING confidence
        "#,
    );

    let new_confidence: f32 = sqlx::query_scalar::<_, f32>(&query)
        .bind(profile)
        .bind(memory_id)
        .bind(delta)
        .bind(now)
        .bind(CONSOLIDATED_TYPES)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ChittaError::InvalidArgument {
                tool: TOOL,
                argument: "memory_id".to_string(),
                constraint: "target memory was modified concurrently".to_string(),
                received: None,
                next_action: "Retry the operation.".to_string(),
            },
            other => other.into(),
        })?;

    // Insert feedback observation row
    let feedback_row_id = Uuid::now_v7();
    let feedback_content = format!("Feedback: {feedback_tag} with memory {memory_id}");
    let feedback_refs = serde_json::json!([
        {"kind": "memory", "ref": memory_id.to_string()}
    ]);
    let feedback_tags = vec!["feedback".to_string(), feedback_tag.to_string()];

    insert_evidence_row(
        &mut tx,
        feedback_row_id,
        profile,
        &feedback_content,
        &feedback_tags,
        &feedback_refs,
        &row.facets,
        now,
    )
    .await?;

    // Insert correction observation row if present
    let correction_row_id = if let Some(corr) = prepared_correction {
        let corr_id = Uuid::now_v7();
        let corr_refs = serde_json::json!([
            {"kind": "memory", "ref": memory_id.to_string()}
        ]);
        let corr_tags = vec!["correction".to_string(), format!("contradicts:{memory_id}")];

        sqlx::query(
            r#"
            INSERT INTO memories
                (id, profile, content, embedding, sparse_embedding,
                 event_time, record_time, idempotency_key, source, memory_type,
                 tags, external_refs, metadata,
                 applies_to_domains, applies_to_skills, applies_to_projects, applies_to_situations,
                 superseded_by, confidence, reinforcement_count, last_reinforced_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
            "#,
        )
        .bind(corr_id)
        .bind(profile)
        .bind(&corr.content)
        .bind(&corr.embedding)
        .bind(&corr.sparse_json)
        .bind(now)
        .bind(now)
        .bind(format!("correction-{}-{}", memory_id, corr_id))
        .bind("record_feedback")
        .bind("observation")
        .bind(&corr_tags)
        .bind(&corr_refs)
        .bind(None::<serde_json::Value>)
        .bind(&row.facets.domains)
        .bind(&row.facets.skills)
        .bind(&row.facets.projects)
        .bind(&row.facets.situations)
        .bind(None::<Uuid>)
        .bind(None::<f32>)
        .bind(0_i32)
        .bind(None::<DateTime<Utc>>)
        .execute(&mut *tx)
        .await?;

        Some(corr_id)
    } else {
        None
    };

    tx.commit().await?;

    Ok(TxResult {
        new_confidence,
        feedback_row_id,
        correction_row_id,
    })
}

async fn insert_evidence_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
    profile: &str,
    content: &str,
    tags: &[String],
    external_refs: &serde_json::Value,
    facets: &Facets,
    now: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO memories
            (id, profile, content, embedding, sparse_embedding,
             event_time, record_time, idempotency_key, source, memory_type,
             tags, external_refs, metadata,
             applies_to_domains, applies_to_skills, applies_to_projects, applies_to_situations,
             superseded_by, confidence, reinforcement_count, last_reinforced_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
        "#,
    )
    .bind(id)
    .bind(profile)
    .bind(content)
    .bind(None::<Vector>)
    .bind(None::<serde_json::Value>)
    .bind(now)
    .bind(now)
    .bind(format!("feedback-{}-{}", profile, id))
    .bind("record_feedback")
    .bind("observation")
    .bind(tags)
    .bind(external_refs)
    .bind(None::<serde_json::Value>)
    .bind(&facets.domains)
    .bind(&facets.skills)
    .bind(&facets.projects)
    .bind(&facets.situations)
    .bind(None::<Uuid>)
    .bind(None::<f32>)
    .bind(0_i32)
    .bind(None::<DateTime<Utc>>)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
