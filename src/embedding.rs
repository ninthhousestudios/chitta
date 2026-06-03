//! BGE-M3 dense embeddings via ONNX Runtime.
//!
//! Contract: one [`Embedder`] per process, shared via `Arc`. Each `embed`
//! call tokenizes, rejects > 8192 tokens, runs the ONNX session, and returns
//! a 1024-dim `Vec<f32>`.
//!
//! # Session pooling
//!
//! The ONNX runtime session (`Session::run` takes `&mut self` in ort
//! 2.0.0-rc.10) is guarded by `std::sync::Mutex` — NOT `tokio::sync::Mutex`,
//! because ort's `Session` is not `Send` across `.await` points. A pool of N
//! independent sessions sits behind a `tokio::sync::Semaphore` that caps
//! concurrency. Tokenization and tensor construction happen outside any lock;
//! only the `session.run()` call holds a session mutex, and it runs inside
//! `spawn_blocking` so the tokio runtime is never blocked.
//!
//! Default pool size is 1 (same memory footprint as v0.0.1). Each additional
//! session loads the full ONNX graph into RAM (~1-2 GB for BGE-M3). Set
//! `CHITTA_EMBEDDER_POOL_SIZE` to scale up when concurrent embedding
//! throughput matters more than memory.
//!
//! # Model output note
//!
//! The `yuniko-software/bge-m3-onnx` export (which the Python chitta reads
//! at `~/.cache/chitta/bge-m3-onnx/bge_m3_model.onnx`) exposes a named output
//! `dense_embeddings` of shape `[batch, 1024]`. Pooling (CLS token) and L2
//! normalization are performed *inside* the exported graph, so the host does
//! no post-processing. The plan doc describes a `[1, seq_len, 1024]` output
//! that the host would pool and normalize — that shape applies to a different
//! BGE-M3 export. We follow the actual behavior of the file on disk.
//!
//! The `sparse_weights` output is ignored in v0.0.1 (no sparse column in the
//! starting shape).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ndarray::Array2;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use tokenizers::Tokenizer;
use tokio::sync::Semaphore;

use crate::error::{ChittaError, Result};

pub struct EmbedOutput {
    pub dense: Vec<f32>,
    pub sparse: HashMap<u32, f32>,
}

static SPARSE_MISSING_WARN: Once = Once::new();
static SPARSE_EXTRACT_WARN: Once = Once::new();

/// Dimension of BGE-M3's dense output. Pinned here so a dimension drift
/// shows up as a loud panic in tests rather than a silent write.
pub const EMBEDDING_DIM: usize = 1024;

/// Hard cap enforced before we run the session (Principle 1: we never
/// embed a truncated version of stored content).
pub const MAX_TOKENS: usize = 8192;

/// Timeout for a single ONNX inference call. If a session.run() takes
/// longer than this, something is seriously wrong (OOM thrashing, etc.).
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Embedder {
    tokenizer: Tokenizer,
    /// Pool of ONNX sessions. Each is independently loadable and guarded by
    /// a blocking `std::sync::Mutex` (not tokio — `Session` is !Send across
    /// `.await`). The `semaphore` limits how many sessions can be in use
    /// concurrently, so `acquire_session` always finds an unlocked slot.
    sessions: Vec<Mutex<Session>>,
    semaphore: Semaphore,
    /// Retained for session replacement and diagnostic logging.
    model_path: PathBuf,
    #[allow(dead_code)]
    tokenizer_path: PathBuf,
    sparse_threshold: f32,
}

impl Embedder {
    /// Load the BGE-M3 model and tokenizer, creating `pool_size` independent
    /// ONNX sessions.
    ///
    /// Each session loads the full ONNX graph into RAM (~1-2 GB for BGE-M3).
    /// A `pool_size` of 1 matches v0.0.1 memory footprint. Increase only
    /// when concurrent embedding throughput justifies the RAM cost.
    pub fn load(
        model_path: &Path,
        tokenizer_path: &Path,
        pool_size: usize,
        sparse_threshold: f32,
    ) -> Result<Arc<Self>> {
        assert!(pool_size >= 1, "embedder pool_size must be >= 1");

        tracing::info!(
            model = ?model_path,
            tokenizer = ?tokenizer_path,
            pool_size,
            "loading BGE-M3"
        );

        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| ChittaError::Embedding {
                tool: "startup",
                message: format!("failed to load tokenizer at {tokenizer_path:?}: {e}"),
                next_action:
                    "Ensure CHITTA_MODEL_PATH contains a valid HuggingFace tokenizer.json."
                        .to_string(),
            })?;

        if let Some(trunc) = tokenizer.get_truncation() {
            tracing::warn!(
                max_length = trunc.max_length,
                "tokenizer.json has truncation enabled — disabling to preserve MAX_TOKENS guard"
            );
            tokenizer
                .with_truncation(None)
                .expect("disabling truncation should not fail");
        }

        let mut sessions = Vec::with_capacity(pool_size);
        for i in 0..pool_size {
            let cuda_ep = ort::execution_providers::CUDAExecutionProvider::default().build();
            let session = Session::builder()
                .map_err(embedding_startup_err)?
                .with_execution_providers([cuda_ep])
                .map_err(embedding_startup_err)?
                .with_optimization_level(GraphOptimizationLevel::Level1)
                .map_err(embedding_startup_err)?
                .commit_from_file(model_path)
                .map_err(|e| ChittaError::Embedding {
                    tool: "startup",
                    message: format!(
                        "failed to load ONNX model (session {i}/{pool_size}) at {model_path:?}: {e}"
                    ),
                    next_action:
                        "Ensure CHITTA_MODEL_PATH contains bge_m3_model.onnx (and its .onnx_data \
                         sidecar) and that libonnxruntime is installed or ORT_DYLIB_PATH is set."
                            .to_string(),
                })?;
            sessions.push(Mutex::new(session));
        }

        Ok(Arc::new(Self {
            tokenizer,
            sessions,
            semaphore: Semaphore::new(pool_size),
            model_path: model_path.to_path_buf(),
            tokenizer_path: tokenizer_path.to_path_buf(),
            sparse_threshold,
        }))
    }

    /// Embed `text` into a 1024-dim dense vector.
    ///
    /// Tokenization and tensor construction are synchronous (fast, no lock).
    /// The ONNX inference runs inside `spawn_blocking` so the tokio runtime
    /// is never blocked by the CPU-bound graph evaluation.
    ///
    /// Takes `self: &Arc<Self>` so we can clone the Arc into the
    /// `spawn_blocking` closure (which requires `Send + 'static`).
    pub async fn embed(self: &Arc<Self>, text: &str, tool: &'static str) -> Result<Vec<f32>> {
        Ok(self.embed_full(text, tool).await?.dense)
    }

    pub async fn embed_full(
        self: &Arc<Self>,
        text: &str,
        tool: &'static str,
    ) -> Result<EmbedOutput> {
        // 1. Tokenize (fast, sync — no lock needed).
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| ChittaError::Embedding {
                tool,
                message: format!("tokenizer error: {e}"),
                next_action:
                    "Tokenizer failed to encode the input. Verify CHITTA_MODEL_PATH contains the \
                     tokenizer.json that matches the deployed BGE-M3 ONNX export."
                        .to_string(),
            })?;

        let ids = encoding.get_ids();
        let attn = encoding.get_attention_mask();

        if ids.len() > MAX_TOKENS {
            return Err(ChittaError::ContentTooLong {
                tool,
                token_count: ids.len(),
            });
        }

        // 2. Build input tensors (fast, sync).
        let seq_len = ids.len();
        let token_ids: Vec<u32> = ids.to_vec();
        let input_ids: Vec<i64> = ids.iter().map(|&id| id as i64).collect();
        let attention_mask: Vec<i64> = attn.iter().map(|&m| m as i64).collect();

        let input_ids_arr = Array2::from_shape_vec((1, seq_len), input_ids)
            .map_err(|e| ChittaError::Internal(format!("failed to build input_ids tensor: {e}")))?;
        let attention_mask_arr =
            Array2::from_shape_vec((1, seq_len), attention_mask).map_err(|e| {
                ChittaError::Internal(format!("failed to build attention_mask tensor: {e}"))
            })?;

        let input_ids_value =
            Value::from_array(input_ids_arr).map_err(|e| ort_to_embed_err(e, tool))?;
        let attention_mask_value =
            Value::from_array(attention_mask_arr).map_err(|e| ort_to_embed_err(e, tool))?;

        // 3. Acquire pool slot (async wait if all sessions are busy).
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| ChittaError::Internal("embedder pool closed".into()))?;

        // 4. Find an available session via round-robin try_lock.
        let session_idx = self.acquire_session();

        // 5. Run inference in a blocking thread with timeout.
        //    Clone the Arc so the closure is 'static + Send. The Mutex guard
        //    is created and consumed entirely within the blocking closure —
        //    it never crosses an .await point.
        let embedder = Arc::clone(self);
        let sparse_threshold = self.sparse_threshold;

        // Returns (inference_result, panicked). The `panicked` flag tells the
        // async context to replace the session slot before propagating the error.
        let blocking_result = tokio::time::timeout(INFERENCE_TIMEOUT, tokio::task::spawn_blocking(
            move || {
                let mut guard = embedder.sessions[session_idx]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());

                // Wrap only session.run() with catch_unwind. ORT sessions can
                // panic internally on certain malformed inputs or internal bugs;
                // we want to catch those and replace the session rather than
                // crash the process.
                let run_result = catch_unwind(AssertUnwindSafe(|| {
                    guard.run(ort::inputs![
                        "input_ids" => input_ids_value,
                        "attention_mask" => attention_mask_value,
                    ])
                }));

                match run_result {
                    Err(_panic_payload) => {
                        // Session panicked — signal the async context to replace it.
                        (
                            Err(ChittaError::Embedding {
                                tool,
                                message: "ONNX session panicked and was replaced".into(),
                                next_action: "Retry the request.".into(),
                            }),
                            true, // panicked
                        )
                    }
                    Ok(Err(ort_err)) => (
                        Err(ChittaError::Embedding {
                            tool,
                            message: format!("ONNX inference failed: {ort_err}"),
                            next_action: "Retry the request. If persistent, check model file integrity.".into(),
                        }),
                        false,
                    ),
                    Ok(Ok(outputs)) => {
                        let embed_result = (|| {
                            let dense = outputs
                                .get("dense_embeddings")
                                .or_else(|| outputs.get("sentence_embedding"))
                                .ok_or_else(|| ChittaError::Embedding {
                                    tool,
                                    message: "ONNX session produced no dense output \
                                        (tried `dense_embeddings`, `sentence_embedding`)"
                                        .to_string(),
                                    next_action: "Report this as a bug; include server logs."
                                        .to_string(),
                                })?;

                            let (shape, data) = dense
                                .try_extract_tensor::<f32>()
                                .map_err(|e| ort_to_embed_err(e, tool))?;

                            // Expected shape is [1, 1024]; accept either [1, 1024] or [1024]
                            // to stay robust against minor export differences.
                            let total: usize = shape.iter().map(|&d| d as usize).product();
                            if total != EMBEDDING_DIM {
                                return Err(ChittaError::Embedding {
                                    tool,
                                    message: format!(
                                        "unexpected embedding shape {shape:?}; expected \
                                         {EMBEDDING_DIM} elements"
                                    ),
                                    next_action: "Report this as a bug; include server logs."
                                        .to_string(),
                                });
                            }

                            let dense_vec = data.to_vec();

                            // Extract sparse lexical weights: expected shape [1, seq_len, 1].
                            let sparse = match outputs.get("sparse_weights") {
                                Some(sw) => {
                                    match sw.try_extract_tensor::<f32>() {
                                        Ok((shape, weights)) => {
                                            let total: usize = shape.iter().map(|&d| d as usize).product();
                                            if total != seq_len {
                                                tracing::warn!(
                                                    ?shape, seq_len,
                                                    "sparse_weights shape mismatch: expected {seq_len} elements, \
                                                     got {total}; sparse embeddings will be empty for this call"
                                                );
                                                HashMap::new()
                                            } else {
                                                let mut map: HashMap<u32, f32> = HashMap::new();
                                                for (pos, &token_id) in token_ids.iter().enumerate() {
                                                    let w = weights[pos];
                                                    if w >= sparse_threshold {
                                                        map.entry(token_id)
                                                            .and_modify(|v| *v = v.max(w))
                                                            .or_insert(w);
                                                    }
                                                }
                                                map
                                            }
                                        }
                                        Err(e) => {
                                            SPARSE_EXTRACT_WARN.call_once(|| {
                                                tracing::warn!(
                                                    "sparse_weights output exists but extraction failed: {e}; \
                                                     sparse embeddings will be empty"
                                                );
                                            });
                                            HashMap::new()
                                        }
                                    }
                                }
                                None => {
                                    SPARSE_MISSING_WARN.call_once(|| {
                                        tracing::warn!(
                                            "ONNX model has no `sparse_weights` output; \
                                             sparse embeddings will be empty"
                                        );
                                    });
                                    HashMap::new()
                                }
                            };

                            Ok(EmbedOutput { dense: dense_vec, sparse })
                        })();
                        (embed_result, false)
                    }
                }
            },
        ))
        .await
        .map_err(|_| ChittaError::Internal(
            "embedding inference timed out (60s limit)".into(),
        ))?
        .map_err(|e| ChittaError::Internal(format!("spawn_blocking failed: {e}")))?;

        let (result, panicked) = blocking_result;
        if panicked {
            tracing::warn!(
                session = session_idx,
                "ONNX session panicked — replacing slot"
            );
            self.replace_session(session_idx);
        }

        result
    }

    /// Find an unlocked session via round-robin `try_lock`. The semaphore
    /// guarantees at least one session is available, so this always succeeds
    /// in practice. Falls back to index 0 if every `try_lock` loses a race.
    pub fn pool_size(&self) -> usize {
        self.sessions.len()
    }

    fn acquire_session(&self) -> usize {
        for i in 0..self.sessions.len() {
            if self.sessions[i].try_lock().is_ok() {
                return i;
            }
        }
        // Semaphore guarantees availability — this is a fallback for the
        // (astronomically unlikely) case where every try_lock loses a race.
        0
    }

    /// Replace a panicked or poisoned session slot with a fresh ONNX session.
    ///
    /// Called by the async context after `spawn_blocking` signals a panic.
    /// If the replacement fails (e.g., model file missing), the slot stays
    /// in whatever state the Mutex is in — other slots continue operating.
    fn replace_session(&self, idx: usize) {
        match Session::builder()
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level1))
            .and_then(|b| b.commit_from_file(&self.model_path))
        {
            Ok(new_session) => {
                *self.sessions[idx].lock().unwrap_or_else(|e| e.into_inner()) = new_session;
                tracing::info!(session = idx, "replacement ONNX session loaded");
            }
            Err(e) => {
                tracing::error!(
                    session = idx,
                    error = %e,
                    "failed to create replacement session — slot degraded"
                );
            }
        }
    }
}

// ── LazyEmbedder ─────────────────────────────────────────────────────

pub struct LazyEmbedder {
    model_path: PathBuf,
    tokenizer_path: PathBuf,
    pool_size: usize,
    sparse_threshold: f32,
    inner: tokio::sync::RwLock<Option<Arc<Embedder>>>,
    load_barrier: tokio::sync::Mutex<()>,
    last_used: AtomicU64,
    idle_ttl: Duration,
}

impl LazyEmbedder {
    pub fn new(
        model_path: PathBuf,
        tokenizer_path: PathBuf,
        pool_size: usize,
        sparse_threshold: f32,
        idle_ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            model_path,
            tokenizer_path,
            pool_size,
            sparse_threshold,
            inner: tokio::sync::RwLock::new(None),
            load_barrier: tokio::sync::Mutex::new(()),
            last_used: AtomicU64::new(0),
            idle_ttl,
        })
    }

    async fn ensure_loaded(&self) -> Result<Arc<Embedder>> {
        // Fast path: model already loaded.
        {
            let guard = self.inner.read().await;
            if let Some(ref e) = *guard {
                self.touch();
                return Ok(Arc::clone(e));
            }
        }
        // Slow path: acquire barrier so only one task loads at a time.
        let _barrier = self.load_barrier.lock().await;
        // Re-check: another task may have loaded while we waited.
        {
            let guard = self.inner.read().await;
            if let Some(ref e) = *guard {
                self.touch();
                return Ok(Arc::clone(e));
            }
        }
        let mp = self.model_path.clone();
        let tp = self.tokenizer_path.clone();
        let ps = self.pool_size;
        let st = self.sparse_threshold;
        let embedder = tokio::task::spawn_blocking(move || Embedder::load(&mp, &tp, ps, st))
            .await
            .map_err(|e| ChittaError::Internal(format!("model load task failed: {e}")))??;
        let mut guard = self.inner.write().await;
        *guard = Some(Arc::clone(&embedder));
        self.touch();
        Ok(embedder)
    }

    fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_used.store(now, Ordering::Relaxed);
    }

    pub async fn embed(self: &Arc<Self>, text: &str, tool: &'static str) -> Result<Vec<f32>> {
        let inner = self.ensure_loaded().await?;
        inner.embed(text, tool).await
    }

    pub async fn embed_full(
        self: &Arc<Self>,
        text: &str,
        tool: &'static str,
    ) -> Result<EmbedOutput> {
        let inner = self.ensure_loaded().await?;
        inner.embed_full(text, tool).await
    }

    pub fn pool_size(&self) -> usize {
        self.pool_size
    }

    pub async fn warm(&self) -> Result<()> {
        self.ensure_loaded().await?;
        tracing::info!("model warmed");
        Ok(())
    }

    pub async fn unload(&self) {
        let mut guard = self.inner.write().await;
        if guard.is_some() {
            *guard = None;
            tracing::info!("model unloaded (idle TTL expired)");
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner
            .try_read()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    pub fn idle_ttl(&self) -> Duration {
        self.idle_ttl
    }

    pub fn spawn_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let ttl_secs = self.idle_ttl.as_secs();
        let check_interval = Duration::from_secs((ttl_secs / 4).clamp(15, 60));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(check_interval).await;
                let Some(this) = weak.upgrade() else {
                    break;
                };
                if !this.is_loaded() {
                    continue;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let last = this.last_used.load(Ordering::Relaxed);
                if last > 0 && now.saturating_sub(last) > ttl_secs {
                    this.unload().await;
                }
            }
        });
    }
}

fn embedding_startup_err(e: ort::Error) -> ChittaError {
    ChittaError::Embedding {
        tool: "startup",
        message: format!("ONNX session builder failed: {e}"),
        next_action:
            "Ensure libonnxruntime is installed (or ORT_DYLIB_PATH is set) and the model files \
             are present."
                .to_string(),
    }
}

fn ort_to_embed_err(e: ort::Error, tool: &'static str) -> ChittaError {
    ChittaError::Embedding {
        tool,
        message: format!("ONNX runtime error: {e}"),
        next_action: "Report this as a bug; include server logs.".to_string(),
    }
}
