//! POST /ingest endpoint and background extraction worker.
//!
//! Fire-and-forget write path for hooks. Not an MCP tool.
//! Pipeline: receive text → queue → split → classify → filter → store.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::Utc;
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db;
use crate::embedding::Embedder;
use crate::error::ChittaError;

#[derive(Debug, Deserialize)]
pub struct IngestRequest {
    pub text: String,
    pub project: Option<String>,
    #[serde(default = "default_profile")]
    pub profile: String,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_profile() -> String {
    "chitta".to_string()
}
fn default_source() -> String {
    "hook:post".to_string()
}

#[derive(Debug, Serialize)]
pub struct IngestResponse {
    pub queued: bool,
    pub queue_id: Uuid,
}

#[derive(Debug)]
pub struct IngestItem {
    pub id: Uuid,
    pub text: String,
    pub project: Option<String>,
    pub profile: String,
    pub source: String,
}

#[derive(Clone)]
pub struct IngestState {
    pub tx: mpsc::Sender<IngestItem>,
}

pub async fn ingest_handler(
    State(state): State<IngestState>,
    Json(req): Json<IngestRequest>,
) -> impl IntoResponse {
    if req.text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "text must be non-empty"})),
        );
    }
    if req.text.len() > 1_000_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "text exceeds 1MB limit"})),
        );
    }

    let queue_id = Uuid::now_v7();
    let item = IngestItem {
        id: queue_id,
        text: req.text,
        project: req.project,
        profile: req.profile,
        source: req.source,
    };

    match state.tx.try_send(item) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!(IngestResponse {
                queued: true,
                queue_id
            })),
        ),
        Err(mpsc::error::TrySendError::Full(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "ingest queue full, try again later"})),
        ),
        Err(mpsc::error::TrySendError::Closed(_)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "ingest worker not running"})),
        ),
    }
}

// --- Extraction worker ---

const NARRATION_PREFIXES: &[&str] = &[
    "let me ",
    "i'll now ",
    "i will now ",
    "let's ",
    "now i'll ",
    "now let me ",
    "i need to ",
    "i should ",
    "first, let me ",
    "next, i'll ",
    "okay, ",
    "alright, ",
];

const ANCHOR_PHRASES: &[(&str, &str)] = &[
    (
        "a technical decision was made about architecture or design",
        "decision",
    ),
    ("a bug was found or a problem was diagnosed", "observation"),
    ("a preference or convention was established", "observation"),
    (
        "an approach was tried and it failed or did not work",
        "observation",
    ),
    (
        "a non-obvious constraint or requirement was discovered",
        "observation",
    ),
    (
        "a correction was made to fix incorrect understanding",
        "decision",
    ),
];

struct AnchorEmbedding {
    embedding: Vec<f32>,
    memory_type: &'static str,
}

pub async fn run_extraction_worker(
    mut rx: mpsc::Receiver<IngestItem>,
    pool: PgPool,
    embedder: Arc<Embedder>,
) {
    tracing::info!("ingest extraction worker started");

    let anchors = match load_anchors(&embedder).await {
        Ok(a) => {
            tracing::info!(count = a.len(), "anchor embeddings loaded");
            a
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to load anchor embeddings, worker exiting");
            return;
        }
    };

    while let Some(item) = rx.recv().await {
        if let Err(e) = process_item(&item, &pool, &embedder, &anchors).await {
            tracing::warn!(
                queue_id = %item.id,
                error = %e,
                "ingest extraction failed for item"
            );
        }
    }

    tracing::info!("ingest extraction worker shutting down");
}

async fn load_anchors(embedder: &Arc<Embedder>) -> Result<Vec<AnchorEmbedding>, ChittaError> {
    let mut anchors = Vec::with_capacity(ANCHOR_PHRASES.len());
    for (phrase, memory_type) in ANCHOR_PHRASES {
        let out = embedder.embed_full(phrase, "anchor_init").await?;
        anchors.push(AnchorEmbedding {
            embedding: out.dense,
            memory_type,
        });
    }
    Ok(anchors)
}

async fn process_item(
    item: &IngestItem,
    pool: &PgPool,
    embedder: &Arc<Embedder>,
    anchors: &[AnchorEmbedding],
) -> Result<(), ChittaError> {
    let sentences = split_chunks(&item.text);
    let mut stored = 0u32;
    let mut seen_hashes: HashSet<String> = HashSet::new();

    for sentence in &sentences {
        let trimmed = sentence.trim();
        if trimmed.len() < 20 {
            continue;
        }
        if is_narration(trimmed) {
            continue;
        }

        let idem_key = make_idempotency_key(trimmed, item.project.as_deref(), &item.source);
        if !seen_hashes.insert(idem_key.clone()) {
            continue;
        }

        let embed_out = match embedder.embed_full(trimmed, "ingest").await {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(error = %e, "skipping chunk that failed to embed");
                continue;
            }
        };

        let (best_type, best_sim) = classify(&embed_out.dense, anchors);
        if best_sim < 0.45 {
            continue;
        }

        let sparse_json = serde_json::to_value(&embed_out.sparse)
            .map_err(|e| ChittaError::Internal(format!("serialize sparse: {e}")))?;
        let sparse_embedding = if embed_out.sparse.is_empty() {
            None
        } else {
            Some(sparse_json)
        };

        let now = Utc::now();
        let row = db::MemoryRow {
            id: Uuid::now_v7(),
            profile: item.profile.clone(),
            content: trimmed.to_string(),
            embedding: Vector::from(embed_out.dense),
            event_time: now,
            record_time: now,
            tags: build_tags(item.project.as_deref(), &item.source),
            idempotency_key: idem_key,
            source: Some(item.source.clone()),
            metadata: None,
            sparse_embedding,
            memory_type: best_type.to_string(),
            external_refs: None,
            invalidated_at: None,
        };

        match db::insert_or_fetch_memory(pool, &row).await {
            Ok((_row, replayed)) => {
                if !replayed {
                    stored += 1;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "failed to store extracted memory");
            }
        }
    }

    if stored > 0 {
        tracing::info!(
            queue_id = %item.id,
            stored,
            total_sentences = sentences.len(),
            "ingest extraction complete"
        );
    }

    Ok(())
}

fn classify<'a>(embedding: &[f32], anchors: &'a [AnchorEmbedding]) -> (&'a str, f32) {
    let mut best_type = "observation";
    let mut best_sim = 0.0f32;
    for anchor in anchors {
        let sim = cosine_similarity(embedding, &anchor.embedding);
        if sim > best_sim {
            best_sim = sim;
            best_type = anchor.memory_type;
        }
    }
    (best_type, best_sim)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn is_narration(s: &str) -> bool {
    let lower = s.to_lowercase();
    NARRATION_PREFIXES.iter().any(|p| lower.starts_with(p))
}

fn make_idempotency_key(chunk: &str, project: Option<&str>, source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(chunk.as_bytes());
    hasher.update(b"|");
    hasher.update(project.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(source.as_bytes());
    let hash = hasher.finalize();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("ingest:{hex}")
}

fn build_tags(project: Option<&str>, source: &str) -> Vec<String> {
    let mut tags = vec!["auto-extracted".to_string()];
    if let Some(p) = project {
        tags.push(format!("project:{p}"));
    }
    tags.push(format!("source:{source}"));
    tags
}

fn split_chunks(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut in_code_fence = false;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            if !current.is_empty() {
                sentences.push(std::mem::take(&mut current));
            }
            continue;
        }
        if in_code_fence {
            continue;
        }

        if trimmed.is_empty() {
            if !current.is_empty() {
                sentences.push(std::mem::take(&mut current));
            }
            continue;
        }

        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(trimmed);

        if ends_with_sentence_boundary(trimmed) {
            sentences.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        sentences.push(current);
    }

    sentences
}

fn ends_with_sentence_boundary(s: &str) -> bool {
    if s.ends_with(". ") || s.ends_with(".\t") {
        return true;
    }
    let s = s.trim_end();
    if !s.ends_with('.') && !s.ends_with('!') && !s.ends_with('?') {
        return false;
    }
    // Don't split on periods in URLs, file paths, or version numbers.
    let last_word = s.rsplit_once(char::is_whitespace).map_or(s, |(_, w)| w);
    if last_word.contains("://") || last_word.contains('/') {
        return false;
    }
    // Version numbers like "v1.2.3." — word has multiple dots
    let dot_count = last_word.chars().filter(|&c| c == '.').count();
    if dot_count > 1 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_basic_chunks() {
        let text = "First sentence. Second sentence. Third.";
        let result = split_chunks(text);
        assert_eq!(result, vec!["First sentence. Second sentence. Third."]);
    }

    #[test]
    fn split_chunks_by_blank_lines() {
        let text = "First paragraph here.\n\nSecond paragraph here.";
        let result = split_chunks(text);
        assert_eq!(
            result,
            vec!["First paragraph here.", "Second paragraph here."]
        );
    }

    #[test]
    fn split_chunks_skips_code_fences() {
        let text = "Before code.\n```\nlet x = 1;\n```\nAfter code.";
        let result = split_chunks(text);
        assert_eq!(result, vec!["Before code.", "After code."]);
    }

    #[test]
    fn narration_detected() {
        assert!(is_narration("Let me check the file now."));
        assert!(is_narration("I'll now read the configuration."));
        assert!(!is_narration("The configuration uses port 3100."));
    }

    #[test]
    fn idempotency_key_stable() {
        let k1 = make_idempotency_key("hello", Some("proj"), "hook:post");
        let k2 = make_idempotency_key("hello", Some("proj"), "hook:post");
        assert_eq!(k1, k2);

        let k3 = make_idempotency_key("hello", Some("other"), "hook:post");
        assert_ne!(k1, k3);
    }

    #[test]
    fn url_not_split() {
        let text = "Visit https://example.com/path.html for more.";
        assert!(!ends_with_sentence_boundary(
            "https://example.com/path.html"
        ));
        let result = split_chunks(text);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn version_not_split() {
        let text = "Using version v1.2.3. It works well.";
        let result = split_chunks(text);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn cosine_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }
}
