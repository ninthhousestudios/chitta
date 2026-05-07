//! Environment-driven configuration.
//!
//! Loaded once at startup via [`Config::from_env`]. No file formats, no
//! runtime reconfiguration — restart to change settings.
//!
//! Default data directory: `~/.chitta/`. Override with `CHITTA_HOME`.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::Result;

const DEFAULT_DATABASE_URL: &str = "postgresql://josh:ogham@localhost/chitta_rs";

/// Search-related configuration bundled to reduce positional-parameter sprawl.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub recency_weight: f32,
    pub recency_half_life_days: f32,
    pub rrf_fts: bool,
    pub rrf_sparse: bool,
    pub rrf_k: u32,
    pub rrf_candidates: i64,
    pub dedup_field: Option<String>,
    pub dedup_fetch_factor: i64,
    pub type_weights: HashMap<String, f32>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub model_path: PathBuf,
    pub log_level: String,
    pub db_max_connections: u32,
    pub db_acquire_timeout_secs: u64,
    pub db_idle_timeout_secs: u64,
    /// Number of independent ONNX sessions in the embedder pool.
    /// Each session loads the full BGE-M3 graph (~1-2 GB RAM). Default 1.
    pub embedder_pool_size: usize,
    /// Whether to log search queries to `query_log` for retrieval research.
    /// Parsed from `CHITTA_QUERY_LOG` env var. Default `true`.
    pub query_log: bool,
    /// HTTP listen address. Parsed from `CHITTA_HTTP_ADDR`. Default `127.0.0.1`.
    pub http_addr: String,
    /// HTTP listen port. Parsed from `CHITTA_HTTP_PORT`. Default `3100`.
    pub http_port: u16,
    pub search: SearchConfig,
    pub sparse_threshold: f32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

        let model_path = std::env::var_os("CHITTA_MODEL_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(default_model_path);

        let log_level = std::env::var("CHITTA_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        let db_max_connections: u32 = parse_env_or("CHITTA_DB_MAX_CONNECTIONS", 8);
        let db_acquire_timeout_secs: u64 = parse_env_or("CHITTA_DB_ACQUIRE_TIMEOUT", 5);
        let db_idle_timeout_secs: u64 = parse_env_or("CHITTA_DB_IDLE_TIMEOUT", 600);

        // Each session loads ~1-2 GB RAM (full BGE-M3 graph). Default 1
        // preserves v0.0.1 memory footprint; increase only when concurrent
        // embedding throughput justifies the RAM cost.
        let embedder_pool_size: usize = parse_env_or("CHITTA_EMBEDDER_POOL_SIZE", 1).max(1); // floor at 1 — zero sessions is nonsensical

        // Default true; only "false" (case-insensitive) disables.
        let query_log: bool = std::env::var("CHITTA_QUERY_LOG")
            .map(|v| !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        let http_addr =
            std::env::var("CHITTA_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());

        let http_port: u16 = parse_env_or("CHITTA_HTTP_PORT", 3100);

        let recency_weight: f32 = parse_env_or("CHITTA_RECENCY_WEIGHT", 0.0);
        let recency_half_life_days: f32 = parse_env_or("CHITTA_RECENCY_HALF_LIFE_DAYS", 30.0);

        let rrf_fts: bool = parse_env_or("CHITTA_RRF_FTS", false);
        let rrf_sparse: bool = parse_env_or("CHITTA_RRF_SPARSE", true);
        let rrf_k: u32 = parse_env_or::<u32>("CHITTA_RRF_K", 60).max(1);
        let rrf_candidates: i64 = parse_env_or::<i64>("CHITTA_RRF_CANDIDATES", 5).max(1);
        let dedup_field: Option<String> = std::env::var("CHITTA_DEDUP_FIELD")
            .ok()
            .filter(|s| !s.is_empty());
        let dedup_fetch_factor: i64 = parse_env_or::<i64>("CHITTA_DEDUP_FETCH_FACTOR", 3).max(1);
        let sparse_threshold: f32 = parse_env_or("CHITTA_SPARSE_THRESHOLD", 0.01);
        let type_weights: HashMap<String, f32> = std::env::var("CHITTA_TYPE_WEIGHTS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| parse_type_weights(&s))
            .unwrap_or_default();

        Ok(Self {
            database_url,
            model_path,
            log_level,
            db_max_connections,
            db_acquire_timeout_secs,
            db_idle_timeout_secs,
            embedder_pool_size,
            query_log,
            http_addr,
            http_port,
            search: SearchConfig {
                recency_weight,
                recency_half_life_days,
                rrf_fts,
                rrf_sparse,
                rrf_k,
                rrf_candidates,
                dedup_field,
                dedup_fetch_factor,
                type_weights,
            },
            sparse_threshold,
        })
    }

    pub fn model_file(&self) -> PathBuf {
        self.model_path.join("bge_m3_model.onnx")
    }

    pub fn tokenizer_file(&self) -> PathBuf {
        self.model_path.join("tokenizer.json")
    }
}

fn parse_type_weights(s: &str) -> HashMap<String, f32> {
    use crate::tools::validate::VALID_MEMORY_TYPES;

    let mut map = HashMap::new();
    for pair in s.split(',') {
        let Some((k, v)) = pair.split_once('=') else {
            tracing::warn!("CHITTA_TYPE_WEIGHTS: ignoring malformed entry {pair:?}");
            continue;
        };
        let key = k.trim();
        let Ok(weight) = v.trim().parse::<f32>() else {
            tracing::warn!(
                "CHITTA_TYPE_WEIGHTS: non-numeric value for {key:?}: {}",
                v.trim()
            );
            continue;
        };
        if !VALID_MEMORY_TYPES.contains(&key) {
            tracing::warn!(
                "CHITTA_TYPE_WEIGHTS: unknown type {key:?} — valid: {VALID_MEMORY_TYPES:?}"
            );
        }
        if weight <= 0.0 {
            tracing::warn!("CHITTA_TYPE_WEIGHTS: weight for {key:?} is {weight} (should be > 0)");
        }
        map.insert(key.to_string(), weight);
    }
    if !map.is_empty() {
        tracing::info!("Type weights: {map:?}");
    }
    map
}

fn parse_env_or<T: std::str::FromStr + std::fmt::Display>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => match v.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                eprintln!(
                    "WARNING: {name}={v:?} is not a valid {ty} — using default {default}",
                    ty = std::any::type_name::<T>(),
                );
                default
            }
        },
    }
}

/// `~/.chitta/`. Overridable via `CHITTA_HOME`.
pub fn chitta_home() -> PathBuf {
    if let Some(v) = std::env::var_os("CHITTA_HOME") {
        return PathBuf::from(v);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".chitta");
        return p;
    }
    PathBuf::from(".chitta")
}

fn default_model_path() -> PathBuf {
    chitta_home().join("models/bge-m3-onnx")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests in this module mutate the process-wide environment. Rust 2024
    // marks `std::env::set_var`/`remove_var` `unsafe` because concurrent env
    // access from other threads is UB. We confine every env mutation to
    // `with_env` below, which:
    //   1. Acquires a module-static Mutex so only one test holds env at a time.
    //   2. Applies the requested deltas, runs the closure, then restores the
    //      prior values regardless of panic outcome.
    // Keeping `unsafe` blocks in exactly one place makes the invariant
    // auditable: every env write in this crate's test code goes through here.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Apply `deltas` (name, value — `None` removes the var) around `f`, then
    /// restore prior values. Serialized via [`ENV_LOCK`]; safe as long as no
    /// other code in the crate mutates env outside this helper.
    fn with_env<R>(deltas: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior: Vec<(String, Option<String>)> = deltas
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        // Safe: ENV_LOCK serializes every env-touching test in this crate.
        unsafe {
            for (k, v) in deltas {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            for (k, v) in &prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn missing_database_url_uses_default() {
        with_env(&[("DATABASE_URL", None)], || {
            let cfg = Config::from_env().unwrap();
            assert_eq!(cfg.database_url, DEFAULT_DATABASE_URL);
        });
    }

    #[test]
    fn defaults_when_only_database_url_set() {
        with_env(
            &[
                ("DATABASE_URL", Some("postgres://localhost/chitta_rs")),
                ("CHITTA_MODEL_PATH", None),
                ("CHITTA_LOG_LEVEL", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.log_level, "info");
                assert!(cfg.model_path.ends_with(".chitta/models/bge-m3-onnx"));
                assert!(cfg.model_file().ends_with("bge_m3_model.onnx"));
                assert!(cfg.tokenizer_file().ends_with("tokenizer.json"));
            },
        );
    }
}
