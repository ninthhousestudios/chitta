//! L2 integration tests: behavior against a live Postgres + ONNX model.
//!
//! **Deviation from plan.** The plan says "spawn the binary and drive stdio
//! with an rmcp client." We drive the tool handlers in-process instead:
//! the library crate already exposes them, subprocess lifecycle adds
//! flakiness for ~zero behavioral coverage above what these tests already
//! check, and JSON-RPC wire framing is exercised separately in
//! `tests/contract.rs`. If Phase 7 adds HTTP or a second client, a
//! subprocess suite earns its keep then.
//!
//! # Running
//!
//! ```bash
//! createdb chitta_rs_test
//! export TEST_DATABASE_URL=postgres://localhost/chitta_rs_test
//! # CHITTA_MODEL_PATH defaults to ~/.cache/chitta/bge-m3-onnx
//! cargo test --test integration
//! ```
//!
//! Tests skip cleanly (print a `SKIPPED:` line and pass) if
//! `TEST_DATABASE_URL` is unset or the model files are missing — so
//! `cargo test` in CI-lite mode still runs unit + contract suites.

use std::path::PathBuf;
use std::sync::Arc;

use chitta_rs::config::{Config, SearchConfig};
use chitta_rs::db;
use chitta_rs::embedding::Embedder;
use chitta_rs::error::ChittaError;
use chitta_rs::tools::{self, DeleteArgs, GetArgs, ListArgs, SearchArgs, StoreArgs, UpdateArgs};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use uuid::Uuid;

// ---- Harness --------------------------------------------------------
//
// Embedder load (~1-2s ONNX startup) is shared via a static because it's a
// pure-sync resource safe to reuse across tests. The DB pool is *not*
// shared: `#[tokio::test]` spins up a fresh runtime per test, and a pool
// created under runtime A has background tasks (reaper, timeout handler)
// pinned to that runtime — when A tears down, other tests see
// `PoolTimedOut`. A fresh per-test pool costs ~20ms and sidesteps the
// whole problem.

struct Harness {
    pool: PgPool,
    embedder: Arc<Embedder>,
    profile: String,
}

/// Shared lazy-loaded embedder. `None` means setup was tried and skipped
/// (missing env var, model file, etc). `OnceCell` serializes the one
/// potentially slow init.
static SHARED: OnceCell<Option<SharedSetup>> = OnceCell::const_new();

#[derive(Clone)]
struct SharedSetup {
    database_url: String,
    embedder: Arc<Embedder>,
}

async fn shared() -> Option<SharedSetup> {
    SHARED.get_or_init(try_shared).await.clone()
}

async fn try_shared() -> Option<SharedSetup> {
    // Best-effort .env load so developers don't have to re-export vars.
    let _ = dotenvy::dotenv();

    let Ok(database_url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: TEST_DATABASE_URL not set");
        return None;
    };

    let model_path: PathBuf = std::env::var_os("CHITTA_MODEL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            let mut p = PathBuf::from(home);
            p.push(".cache/chitta/bge-m3-onnx");
            p
        });

    let cfg = Config {
        database_url: database_url.clone(),
        model_path,
        log_level: "warn".into(),
        db_max_connections: 8,
        db_acquire_timeout_secs: 5,
        db_idle_timeout_secs: 600,
        embedder_pool_size: 1,
        query_log: false,
        http_addr: "127.0.0.1".into(),
        http_port: 3100,
        search: SearchConfig {
            recency_weight: 0.0,
            recency_half_life_days: 30.0,
            rrf_fts: false,
            rrf_sparse: false,
            rrf_k: 60,
            rrf_candidates: 5,
            dedup_field: None,
            dedup_fetch_factor: 3,
            type_weights: std::collections::HashMap::new(),
        },
        sparse_threshold: 0.01,
    };

    if !cfg.model_file().is_file() || !cfg.tokenizer_file().is_file() {
        eprintln!(
            "SKIPPED: model or tokenizer missing at {:?}",
            cfg.model_path
        );
        return None;
    }

    // Run migrations once up front against a short-lived pool, so per-test
    // pools don't race `_sqlx_migrations`.
    let bootstrap_pool = match db::connect(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIPPED: cannot connect to TEST_DATABASE_URL: {e}");
            return None;
        }
    };
    if let Err(e) = db::run_migrations(&bootstrap_pool).await {
        eprintln!("SKIPPED: migration failed: {e}");
        return None;
    }
    drop(bootstrap_pool);

    let embedder = match Embedder::load(&cfg.model_file(), &cfg.tokenizer_file(), 1, 0.01) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("SKIPPED: embedder failed to load: {e:?}");
            return None;
        }
    };

    Some(SharedSetup {
        database_url,
        embedder,
    })
}

async fn fresh_harness(name: &str) -> Option<Harness> {
    let s = shared().await?;
    let cfg = Config {
        database_url: s.database_url,
        model_path: PathBuf::new(), // unused past embedder load
        log_level: "warn".into(),
        db_max_connections: 8,
        db_acquire_timeout_secs: 5,
        db_idle_timeout_secs: 600,
        embedder_pool_size: 1,
        query_log: false,
        http_addr: "127.0.0.1".into(),
        http_port: 3100,
        search: SearchConfig {
            recency_weight: 0.0,
            recency_half_life_days: 30.0,
            rrf_fts: false,
            rrf_sparse: false,
            rrf_k: 60,
            rrf_candidates: 5,
            dedup_field: None,
            dedup_fetch_factor: 3,
            type_weights: std::collections::HashMap::new(),
        },
        sparse_threshold: 0.01,
    };
    let pool = match db::connect(&cfg).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIPPED: per-test pool failed: {e}");
            return None;
        }
    };
    Some(Harness {
        pool,
        embedder: s.embedder,
        profile: unique_profile(name),
    })
}

/// Unique profile per test so parallel tests (and reruns) don't collide.
fn unique_profile(name: &str) -> String {
    format!("it_{name}_{}", Uuid::now_v7().simple())
}

/// Macro for the skip-or-run dance. Use as the first line of every test.
macro_rules! require_harness {
    ($name:expr) => {
        match fresh_harness($name).await {
            Some(h) => h,
            None => return,
        }
    };
}

fn test_search_cfg() -> SearchConfig {
    SearchConfig {
        recency_weight: 0.0,
        recency_half_life_days: 30.0,
        rrf_fts: false,
        rrf_sparse: false,
        rrf_k: 60,
        rrf_candidates: 5,
        dedup_field: None,
        dedup_fetch_factor: 3,
        type_weights: std::collections::HashMap::new(),
    }
}

// ---- Tests ----------------------------------------------------------

#[tokio::test]
async fn idempotent_replay_returns_same_row() {
    let h = require_harness!("idem");
    let profile = h.profile.clone();

    let args = || StoreArgs {
        profile: profile.clone(),
        content: "memory one".into(),
        idempotency_key: "k-1".into(),
        event_time: None,
        tags: None,
        source: None,
        metadata: None,
        memory_type: None,
        external_refs: None,
    };

    let first = tools::store::handle(&h.pool, h.embedder.clone(), args())
        .await
        .unwrap();
    assert!(!first.idempotent_replay);

    let second = tools::store::handle(&h.pool, h.embedder.clone(), args())
        .await
        .unwrap();
    let third = tools::store::handle(&h.pool, h.embedder.clone(), args())
        .await
        .unwrap();

    assert!(second.idempotent_replay);
    assert!(third.idempotent_replay);
    assert_eq!(first.id, second.id);
    assert_eq!(first.id, third.id);

    // Exactly one row in the DB for this (profile, idempotency_key).
    let (count,): (i64,) = sqlx::query_as(
        "select count(*)::bigint from memories where profile = $1 and idempotency_key = $2",
    )
    .bind(&profile)
    .bind("k-1")
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn verbatim_roundtrip_preserves_unicode_and_whitespace() {
    let h = require_harness!("verbatim");
    let profile = h.profile.clone();

    let content = "  hello\t 世界 🌏 \n trailing ";
    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: content.into(),
            idempotency_key: "v-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let fetched = tools::get::handle(
        &h.pool,
        GetArgs {
            profile: profile.clone(),
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        fetched.content, content,
        "content must round-trip byte-for-byte"
    );
}

#[tokio::test]
async fn search_envelope_has_four_fields_on_empty_profile() {
    let h = require_harness!("empty");

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "nothing will match".into(),
            k: None,
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(out.results.is_empty());
    assert!(!out.truncated);
    assert_eq!(out.total_available, Some(0));
    assert!(
        out.budget_spent_tokens > 0,
        "envelope overhead must be counted"
    );
}

#[tokio::test]
async fn search_max_tokens_triggers_truncated_with_honest_total() {
    let h = require_harness!("budget");
    let profile = h.profile.clone();

    // Seed five memories; semantic content varies but all should match a
    // generic query.
    for i in 0..5 {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: profile.clone(),
                content: format!("fact number {i}: the quick brown fox jumps"),
                idempotency_key: format!("b-{i}"),
                event_time: None,
                tags: None,
                source: None,
                metadata: None,
                memory_type: None,
                external_refs: None,
            },
        )
        .await
        .unwrap();
    }

    // Tiny cap — should hold exactly the first result and flag truncated.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "quick fox".into(),
            k: None,
            max_tokens: Some(1),
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(
        out.truncated,
        "expected truncated=true under tight max_tokens"
    );
    assert_eq!(out.results.len(), 1, "apply_budget keeps at least one hit");
    assert!(
        out.total_available.unwrap() >= out.results.len() as u64,
        "total_available must be >= results.len()"
    );
}

#[tokio::test]
async fn error_contract_invalid_event_time_populates_next_action() {
    let h = require_harness!("bad_time");

    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "anything".into(),
            idempotency_key: "e-1".into(),
            event_time: Some(
                chrono::Utc
                    .with_ymd_and_hms(1969, 6, 20, 0, 0, 0)
                    .single()
                    .unwrap(),
            ),
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap_err();

    let data = err.data();
    assert_eq!(data.tool, "store_memory");
    assert_eq!(data.argument.as_deref(), Some("event_time"));
    assert!(!data.constraint.is_empty());
    assert!(!data.next_action.is_empty());
    assert!(data.next_action.contains("1970") || data.next_action.contains("record_time"));
}

#[tokio::test]
async fn error_contract_not_found_points_at_search() {
    let h = require_harness!("miss");

    let err = tools::get::handle(
        &h.pool,
        GetArgs {
            profile: h.profile.clone(),
            id: Uuid::now_v7().to_string(),
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
    let data = err.data();
    assert_eq!(data.tool, "get_memory");
    assert!(data.next_action.contains("search_memories"));
}

#[tokio::test]
async fn search_snippet_is_verbatim_prefix() {
    let h = require_harness!("snip");
    let profile = h.profile.clone();

    // Content longer than 200 chars so the prefix is an actual truncation.
    let content: String = "α".repeat(300);
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: content.clone(),
            idempotency_key: "s-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "alpha".into(),
            k: None,
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(out.results.len(), 1);
    let snippet = &out.results[0].snippet;
    assert_eq!(snippet.chars().count(), 200);
    assert!(
        content.starts_with(snippet),
        "snippet must be a verbatim prefix"
    );
}

#[tokio::test]
async fn profile_isolation_keeps_searches_scoped() {
    let h = require_harness!("iso_a");
    let profile_a = h.profile.clone();
    let profile_b = unique_profile("iso_b");

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile_a.clone(),
            content: "unique sentinel content zebra".into(),
            idempotency_key: "a-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let in_b = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: profile_b,
            query: "zebra".into(),
            k: None,
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(in_b.total_available, Some(0));
    assert!(in_b.results.is_empty());
}

#[tokio::test]
async fn content_too_long_rejected_with_token_count() {
    let h = require_harness!("long");

    // "alpha " repeats to ~15k tokens (tokenizer varies, but well over 8192).
    let content = "alpha ".repeat(15000);
    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content,
            idempotency_key: "l-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::ContentTooLong { token_count, .. } => {
            assert!(*token_count > 8192, "token_count reported: {token_count}");
        }
        other => panic!("expected ContentTooLong, got {other:?}"),
    }
    let data = err.data();
    assert!(data.next_action.contains("7500"));
}

#[tokio::test]
async fn concurrent_duplicate_writes_converge_on_one_row() {
    let h = require_harness!("conc");
    let profile = h.profile.clone();

    let args = || StoreArgs {
        profile: profile.clone(),
        content: "race-condition content".into(),
        idempotency_key: "c-1".into(),
        event_time: None,
        tags: None,
        source: None,
        metadata: None,
        memory_type: None,
        external_refs: None,
    };

    let (a, b) = tokio::join!(
        tools::store::handle(&h.pool, h.embedder.clone(), args()),
        tools::store::handle(&h.pool, h.embedder.clone(), args()),
    );
    let a = a.expect("first call must succeed");
    let b = b.expect("second call must succeed");

    assert_eq!(a.id, b.id, "both calls must resolve to the same memory id");
    assert!(
        a.idempotent_replay || b.idempotent_replay,
        "at least one call must report idempotent_replay=true"
    );

    let (count,): (i64,) = sqlx::query_as(
        "select count(*)::bigint from memories where profile = $1 and idempotency_key = $2",
    )
    .bind(&profile)
    .bind("c-1")
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "exactly one row survives the race");
}

#[tokio::test]
async fn search_finds_stored_memory_by_semantic_similarity() {
    let h = require_harness!("sem");
    let profile = h.profile.clone();

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "Postgres connection pooling best practices under heavy load".into(),
            idempotency_key: "sem-1".into(),
            event_time: None,
            tags: Some(vec!["db".into(), "perf".into()]),
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "postgres pool tuning".into(),
            k: None,
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(!out.results.is_empty());
    let top = &out.results[0];
    assert_eq!(top.id, stored.id);
    assert!(
        top.similarity > 0.5,
        "expected strong similarity, got {}",
        top.similarity
    );
    assert!(top.tags.contains(&"db".to_string()));
}

// ---- v0.0.2 tests -----------------------------------------------------

#[tokio::test]
async fn update_memory_content_reembeds() {
    let h = require_harness!("upd_content");
    let profile = h.profile.clone();

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "original content about databases".into(),
            idempotency_key: "uc-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let updated = tools::update::handle(
        &h.pool,
        h.embedder.clone(),
        UpdateArgs {
            profile: profile.clone(),
            id: stored.id.to_string(),
            content: Some("completely new content about cooking".into()),
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(updated.id, stored.id);
    assert!(updated.re_embedded, "content change must trigger re-embed");

    let fetched = tools::get::handle(
        &h.pool,
        GetArgs {
            profile,
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(fetched.content, "completely new content about cooking");
}

#[tokio::test]
async fn update_memory_tags_only_no_reembed() {
    let h = require_harness!("upd_tags");
    let profile = h.profile.clone();

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "tags-only update test content".into(),
            idempotency_key: "ut-1".into(),
            event_time: None,
            tags: Some(vec!["old".into()]),
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let updated = tools::update::handle(
        &h.pool,
        h.embedder.clone(),
        UpdateArgs {
            profile: profile.clone(),
            id: stored.id.to_string(),
            content: None,
            tags: Some(vec!["new-tag".into(), "another".into()]),
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(updated.id, stored.id);
    assert!(!updated.re_embedded, "tags-only update must not re-embed");
    assert_eq!(updated.content, "tags-only update test content");
    assert_eq!(
        updated.tags,
        vec!["new-tag".to_string(), "another".to_string()]
    );
}

#[tokio::test]
async fn update_memory_not_found() {
    let h = require_harness!("upd_miss");

    let err = tools::update::handle(
        &h.pool,
        h.embedder.clone(),
        UpdateArgs {
            profile: h.profile.clone(),
            id: Uuid::now_v7().to_string(),
            content: Some("anything".into()),
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn update_memory_requires_at_least_one_field() {
    let h = require_harness!("upd_empty");

    let err = tools::update::handle(
        &h.pool,
        h.embedder.clone(),
        UpdateArgs {
            profile: h.profile.clone(),
            id: Uuid::now_v7().to_string(),
            content: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::InvalidArgument { argument, .. } => {
            assert!(
                argument.contains("content") || argument.contains("tags"),
                "error should mention content/tags, got: {argument}"
            );
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[tokio::test]
async fn soft_delete_hides_memory() {
    let h = require_harness!("del_ok");
    let profile = h.profile.clone();

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "memory to delete".into(),
            idempotency_key: "d-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let del = tools::delete::handle(
        &h.pool,
        DeleteArgs {
            profile: profile.clone(),
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert!(del.deleted);
    assert_eq!(del.id, stored.id);

    // get should now fail with NotFound.
    let err = tools::get::handle(
        &h.pool,
        GetArgs {
            profile,
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound after delete, got {other:?}"),
    }
}

#[tokio::test]
async fn restore_after_soft_delete_creates_new_row() {
    let h = require_harness!("del_restore");
    let profile = h.profile.clone();

    let first = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "original content".into(),
            idempotency_key: "reuse-key".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();
    assert!(!first.idempotent_replay);

    tools::delete::handle(
        &h.pool,
        DeleteArgs {
            profile: profile.clone(),
            id: first.id.to_string(),
        },
    )
    .await
    .unwrap();

    let second = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "replacement content".into(),
            idempotency_key: "reuse-key".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    assert!(!second.idempotent_replay, "should be a fresh store, not a replay");
    assert_ne!(second.id, first.id, "new row should have a different id");
    assert_eq!(second.content, "replacement content");
}

#[tokio::test]
async fn delete_memory_not_found() {
    let h = require_harness!("del_miss");

    let err = tools::delete::handle(
        &h.pool,
        DeleteArgs {
            profile: h.profile.clone(),
            id: Uuid::now_v7().to_string(),
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn list_recent_returns_time_ordered() {
    let h = require_harness!("list_ord");
    let profile = h.profile.clone();

    // Store 3 memories sequentially to get distinct record_times.
    for i in 0..3 {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: profile.clone(),
                content: format!("list order memory {i}"),
                idempotency_key: format!("lo-{i}"),
                event_time: None,
                tags: None,
                source: None,
                metadata: None,
                memory_type: None,
                external_refs: None,
            },
        )
        .await
        .unwrap();
    }

    let out = tools::list::handle(
        &h.pool,
        ListArgs {
            profile,
            limit: None,
            tags: None,
            memory_types: None,
        },
    )
    .await
    .unwrap();

    assert!(out.memories.len() >= 3);
    // Verify DESC ordering: each record_time >= the next.
    for pair in out.memories.windows(2) {
        assert!(
            pair[0].record_time >= pair[1].record_time,
            "list must be ordered by record_time DESC: {:?} before {:?}",
            pair[0].record_time,
            pair[1].record_time,
        );
    }
}

#[tokio::test]
async fn list_recent_respects_limit() {
    let h = require_harness!("list_lim");
    let profile = h.profile.clone();

    for i in 0..3 {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: profile.clone(),
                content: format!("limit test memory {i}"),
                idempotency_key: format!("ll-{i}"),
                event_time: None,
                tags: None,
                source: None,
                metadata: None,
                memory_type: None,
                external_refs: None,
            },
        )
        .await
        .unwrap();
    }

    let out = tools::list::handle(
        &h.pool,
        ListArgs {
            profile,
            limit: Some(2),
            tags: None,
            memory_types: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(out.memories.len(), 2, "limit=2 must return exactly 2");
    assert!(
        out.total_in_profile >= 3,
        "total_in_profile must reflect all stored"
    );
}

#[tokio::test]
async fn search_with_tag_filter_returns_only_matching() {
    let h = require_harness!("tag_filter");
    let profile = h.profile.clone();

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "Rust async runtime with tokio".into(),
            idempotency_key: "tf-1".into(),
            event_time: None,
            tags: Some(vec!["rust".into()]),
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "Python asyncio event loop".into(),
            idempotency_key: "tf-2".into(),
            event_time: None,
            tags: Some(vec!["python".into()]),
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "async programming".into(),
            k: None,
            max_tokens: None,
            tags: Some(vec!["rust".into()]),
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(
        !out.results.is_empty(),
        "should find the rust-tagged memory"
    );
    for hit in &out.results {
        assert!(
            hit.tags.contains(&"rust".to_string()),
            "all results must have the 'rust' tag, got tags: {:?}",
            hit.tags,
        );
    }
}

#[tokio::test]
async fn search_with_min_similarity_filters_low_scores() {
    let h = require_harness!("min_sim");
    let profile = h.profile.clone();

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile.clone(),
            content: "Rust async concurrency with tokio and futures".into(),
            idempotency_key: "ms-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "French cooking recipes with butter and garlic".into(),
            k: None,
            max_tokens: None,
            tags: None,
            min_similarity: Some(0.8),
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        out.results.len(),
        0,
        "unrelated query with high min_similarity should return 0 results"
    );
}

#[tokio::test]
async fn truncated_false_when_all_results_fit() {
    let h = require_harness!("trunc_false");
    let profile = h.profile.clone();

    for i in 0..2 {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: profile.clone(),
                content: format!("truncation regression memory {i}"),
                idempotency_key: format!("tr-{i}"),
                event_time: None,
                tags: None,
                source: None,
                metadata: None,
                memory_type: None,
                external_refs: None,
            },
        )
        .await
        .unwrap();
    }

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile,
            query: "truncation regression".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(
        !out.truncated,
        "truncated must be false when results.len() < k (Issue 10 regression)"
    );
    assert!(out.results.len() <= 2);
}

#[tokio::test]
async fn get_memory_cross_profile_isolation() {
    let h = require_harness!("xprofile");
    let profile_a = h.profile.clone();
    let profile_b = unique_profile("xprofile_b");

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: profile_a,
            content: "cross-profile isolation test".into(),
            idempotency_key: "xp-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    // Same UUID, different profile — must not find it.
    let err = tools::get::handle(
        &h.pool,
        GetArgs {
            profile: profile_b,
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound for wrong profile, got {other:?}"),
    }
}

// Pull chrono::TimeZone into scope for the event_time test without polluting
// the top of the file.
use chrono::TimeZone;

// ---- memory_type behavioral tests ------------------------------------

#[tokio::test]
async fn store_with_non_default_memory_type_roundtrips() {
    let h = require_harness!("mt_store");

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers evidence over intuition for architecture decisions.".into(),
            idempotency_key: "mt-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(stored.memory_type, "observation");

    let fetched = tools::get::handle(
        &h.pool,
        GetArgs {
            profile: h.profile.clone(),
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap();

    assert_eq!(fetched.memory_type, "observation");
}

#[tokio::test]
async fn search_memory_types_filter_excludes_non_matching() {
    let h = require_harness!("mt_filter");

    for (key, content, mt) in [
        ("f-1", "The sun is a star.", "memory"),
        (
            "f-2",
            "Josh corrected the approach — prefers benchmarks first.",
            "observation",
        ),
        (
            "f-3",
            "Decided to use RRF with k=60 for retrieval.",
            "decision",
        ),
    ] {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: content.into(),
                idempotency_key: key.into(),
                event_time: None,
                tags: None,
                source: None,
                metadata: None,
                memory_type: Some(mt.into()),
                external_refs: None,
            },
        )
        .await
        .unwrap();
    }

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "decision about retrieval".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: Some(vec!["decision".into()]),
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    for hit in &out.results {
        assert_eq!(
            hit.memory_type, "decision",
            "filter should exclude non-decision types"
        );
    }
    assert!(!out.results.is_empty(), "should find at least one decision");
}

#[tokio::test]
async fn invalid_memory_type_store_rejected() {
    let h = require_harness!("mt_invalid");

    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "test".into(),
            idempotency_key: "bad-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: Some("bogus".into()),
            external_refs: None,
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "memory_type");
        }
        other => panic!("expected InvalidArgument for memory_type, got {other:?}"),
    }
}

#[tokio::test]
async fn search_returns_score_and_similarity() {
    let h = require_harness!("mt_score");

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Rust programming language is great for systems.".into(),
            idempotency_key: "sc-1".into(),
            event_time: None,
            tags: None,
            source: None,
            metadata: None,
            memory_type: None,
            external_refs: None,
        },
    )
    .await
    .unwrap();

    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "Rust systems programming".into(),
            k: Some(5),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_retired: None,
            ref_filter: None,
        },
    )
    .await
    .unwrap();

    assert!(!out.results.is_empty());
    let hit = &out.results[0];
    assert!(hit.similarity > 0.0, "similarity should be raw cosine > 0");
    assert!(hit.score > 0.0, "score should be > 0");
    assert_eq!(
        hit.similarity, hit.score,
        "with no weights/recency, score == similarity"
    );
}
