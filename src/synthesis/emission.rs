use std::sync::Arc;

use chrono::{DateTime, Utc};
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

fn embed_and_build(
    embed_out: &crate::embedding::EmbedOutput,
    profile: &str,
    content: String,
    idempotency_key: String,
    memory_type: String,
    tags: Vec<String>,
    facets: Facets,
    confidence: f32,
    now: DateTime<Utc>,
) -> Result<MemoryRow> {
    let sparse_json = serde_json::to_value(&embed_out.sparse)
        .map_err(|e| ChittaError::Internal(format!("sparse serialization: {e}")))?;

    Ok(MemoryRow::new_emission(
        profile,
        content,
        idempotency_key,
        memory_type,
        tags,
        facets,
        confidence,
        embed_out.dense.clone(),
        sparse_json,
        now,
    ))
}

fn cluster_idem_key(cluster: &Cluster) -> String {
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
    format!(
        "reflect-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3],
        hash[4], hash[5], hash[6], hash[7],
        hash[8], hash[9], hash[10], hash[11],
        hash[12], hash[13], hash[14], hash[15],
    )
}

pub async fn emit_consolidated_auto(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    cluster: &Cluster,
    profile: &str,
    now: DateTime<Utc>,
    source_facets: Facets,
) -> Result<(MemoryRow, bool)> {
    let embed_out = embedder
        .embed_full(&cluster.representative_claim, "reflect")
        .await?;
    emit_consolidated(pool, &embed_out, cluster, profile, now, source_facets).await
}

pub(crate) async fn emit_consolidated(
    pool: &PgPool,
    claim_embedding: &crate::embedding::EmbedOutput,
    cluster: &Cluster,
    profile: &str,
    now: DateTime<Utc>,
    source_facets: Facets,
) -> Result<(MemoryRow, bool)> {
    let row = embed_and_build(
        claim_embedding,
        profile,
        cluster.representative_claim.clone(),
        cluster_idem_key(cluster),
        cluster.memory_type.clone(),
        vec!["reflect".into(), "synthesised".into()],
        source_facets,
        emission_confidence(cluster.source_ids.len()),
        now,
    )?;

    let derivations: Vec<(Uuid, String)> = cluster
        .source_ids
        .iter()
        .map(|&sid| (sid, "synthesised_from".into()))
        .collect();

    db::insert_memory_with_derivations(pool, &row, &derivations).await
}

pub(crate) async fn emit_with_supersession(
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    claim_embedding: &crate::embedding::EmbedOutput,
    cluster: &Cluster,
    contradiction: &Contradiction,
    profile: &str,
    now: DateTime<Utc>,
    source_facets: Facets,
) -> Result<SupersessionResult> {
    let (new_row, idempotent_replay) =
        emit_consolidated(pool, claim_embedding, cluster, profile, now, source_facets.clone())
            .await?;

    let old_row = db::get_memory_by_id(pool, profile, contradiction.existing_id).await?;
    let already_superseded = old_row.as_ref().and_then(|r| r.superseded_by).is_some();

    if !already_superseded {
        db::supersede_memory(pool, contradiction.existing_id, new_row.id).await?;
    }

    let meta_content = format!(
        "Josh shifted from '{}' to '{}' — {}",
        contradiction.existing_claim, cluster.representative_claim, contradiction.shift_description,
    );
    let meta_idem_key = format!("reflect-meta-{}-{}", contradiction.existing_id, new_row.id);

    let embed_out = embedder.embed_full(&meta_content, "reflect").await?;

    let meta_row = embed_and_build(
        &embed_out,
        profile,
        meta_content,
        meta_idem_key,
        "supersession_record".into(),
        vec![
            "reflect".into(),
            "synthesised".into(),
            "supersession".into(),
        ],
        source_facets,
        emission_confidence(cluster.source_ids.len()),
        now,
    )?;

    let derivations = vec![
        (contradiction.existing_id, "supersession_of".into()),
        (new_row.id, "supersession_to".into()),
    ];

    let meta_row_id = meta_row.id;
    let (_, meta_replay) =
        db::insert_memory_with_derivations(pool, &meta_row, &derivations).await?;

    Ok(SupersessionResult {
        superseded_id: contradiction.existing_id,
        meta_row_id,
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
