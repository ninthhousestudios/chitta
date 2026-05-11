use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::Vector;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::{Cluster, Contradiction, SupersessionResult};
use crate::db::{self, MemoryRow};
use crate::embedding::Embedder;
use crate::error::{ChittaError, Result};
use crate::facets::Facets;

pub fn emission_confidence(cluster_size: usize) -> f32 {
    (0.50 + 0.05 * cluster_size as f32).min(0.90)
}

pub async fn emit_consolidated(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    cluster: &Cluster,
    profile: &str,
    now: DateTime<Utc>,
    source_facets: Facets,
) -> Result<(MemoryRow, bool)> {
    let confidence = emission_confidence(cluster.source_ids.len());
    let id = Uuid::now_v7();

    let mut sorted_ids = cluster.source_ids.clone();
    sorted_ids.sort();
    let key_material = format!(
        "reflect:{}:{}",
        cluster.representative_claim,
        sorted_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let hash = Sha256::digest(key_material.as_bytes());
    let idem_key = format!(
        "reflect-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0],
        hash[1],
        hash[2],
        hash[3],
        hash[4],
        hash[5],
        hash[6],
        hash[7],
        hash[8],
        hash[9],
        hash[10],
        hash[11],
        hash[12],
        hash[13],
        hash[14],
        hash[15]
    );

    let embed_out = embedder
        .embed_full(&cluster.representative_claim, "reflect")
        .await?;
    let sparse_json = serde_json::to_value(&embed_out.sparse)
        .map_err(|e| ChittaError::Internal(format!("sparse serialization: {e}")))?;

    let row = MemoryRow {
        id,
        profile: profile.to_string(),
        content: cluster.representative_claim.clone(),
        embedding: Some(Vector::from(embed_out.dense)),
        sparse_embedding: Some(sparse_json),
        event_time: now,
        record_time: now,
        idempotency_key: idem_key,
        source: Some("reflect".into()),
        memory_type: cluster.memory_type.clone(),
        tags: vec!["reflect".into(), "synthesised".into()],
        external_refs: None,
        metadata: None,
        facets: source_facets,
        superseded_by: None,
        confidence: Some(confidence),
        reinforcement_count: 0,
        last_reinforced_at: None,
        invalidated_at: None,
    };

    let derivations: Vec<(Uuid, String)> = cluster
        .source_ids
        .iter()
        .map(|&sid| (sid, "synthesised_from".into()))
        .collect();

    db::insert_memory_with_derivations(pool, &row, &derivations).await
}

pub async fn emit_with_supersession(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    cluster: &Cluster,
    contradiction: &Contradiction,
    profile: &str,
    now: DateTime<Utc>,
    source_facets: Facets,
) -> Result<SupersessionResult> {
    let (new_row, idempotent_replay) =
        emit_consolidated(pool, embedder, cluster, profile, now, source_facets.clone()).await?;

    let old_row = db::get_memory_by_id(pool, profile, contradiction.existing_id).await?;
    let already_superseded = old_row.as_ref().and_then(|r| r.superseded_by).is_some();

    if !already_superseded {
        db::supersede_memory(pool, contradiction.existing_id, new_row.id).await?;
    }

    let meta_content = format!(
        "Josh shifted from '{}' to '{}' — {}",
        contradiction.existing_claim, cluster.representative_claim, contradiction.shift_description,
    );
    let meta_id = Uuid::now_v7();
    let meta_idem_key = format!("reflect-meta-{}-{}", contradiction.existing_id, new_row.id);

    let embed_out = embedder.embed_full(&meta_content, "reflect").await?;
    let sparse_json = serde_json::to_value(&embed_out.sparse)
        .map_err(|e| ChittaError::Internal(format!("sparse serialization: {e}")))?;

    let meta_row = MemoryRow {
        id: meta_id,
        profile: profile.to_string(),
        content: meta_content,
        embedding: Some(Vector::from(embed_out.dense)),
        sparse_embedding: Some(sparse_json),
        event_time: now,
        record_time: now,
        idempotency_key: meta_idem_key,
        source: Some("reflect".into()),
        memory_type: "supersession_record".into(),
        tags: vec![
            "reflect".into(),
            "synthesised".into(),
            "supersession".into(),
        ],
        external_refs: None,
        metadata: None,
        facets: source_facets,
        superseded_by: None,
        confidence: Some(emission_confidence(cluster.source_ids.len())),
        reinforcement_count: 0,
        last_reinforced_at: None,
        invalidated_at: None,
    };

    let derivations = vec![
        (contradiction.existing_id, "supersession_of".into()),
        (new_row.id, "supersession_to".into()),
    ];

    let (_, meta_replay) =
        db::insert_memory_with_derivations(pool, &meta_row, &derivations).await?;

    Ok(SupersessionResult {
        superseded_id: contradiction.existing_id,
        meta_row_id: meta_id,
        new_row,
        idempotent_replay: idempotent_replay && already_superseded && meta_replay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_at_min_cluster() {
        assert!((emission_confidence(5) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn confidence_scales_with_size() {
        assert!((emission_confidence(6) - 0.80).abs() < 1e-6);
        assert!((emission_confidence(7) - 0.85).abs() < 1e-6);
    }

    #[test]
    fn confidence_caps_at_090() {
        assert!((emission_confidence(8) - 0.90).abs() < 1e-6);
        assert!((emission_confidence(20) - 0.90).abs() < 1e-6);
    }
}
