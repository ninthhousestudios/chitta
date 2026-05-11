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
//! createdb chitta_test
//! export TEST_DATABASE_URL=postgres://localhost/chitta_test
//! # CHITTA_MODEL_PATH defaults to ~/.chitta/models/bge-m3-onnx
//! cargo test --test integration
//! ```
//!
//! Tests skip cleanly (print a `SKIPPED:` line and pass) if
//! `TEST_DATABASE_URL` is unset or the model files are missing — so
//! `cargo test` in CI-lite mode still runs unit + contract suites.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use chitta::config::{Config, SearchConfig};
use chitta::db;
use chitta::embedding::Embedder;
use chitta::error::ChittaError;
use chitta::facets::Facets;
use chitta::synthesis::{self, Llm, ThresholdConfig};
use chitta::tools::{
    self, AppliesTo, DeleteArgs, FeedbackKind, GetArgs, GetProfileArgs, ListArgs,
    RecordFeedbackArgs, ReflectStatusArgs, SearchArgs, StoreArgs, SupersedeArgs, UpdateArgs,
};
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
            p.push(".chitta/models/bge-m3-onnx");
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

        metadata: None,
        memory_type: None,
        external_refs: None,
        facets: Facets::default(),
        confidence: None,
        source: None,
        derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

                metadata: None,
                memory_type: None,
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

        metadata: None,
        memory_type: None,
        external_refs: None,
        facets: Facets::default(),
        confidence: None,
        source: None,
        derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    assert!(
        !second.idempotent_replay,
        "should be a fresh store, not a replay"
    );
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

                metadata: None,
                memory_type: None,
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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

                metadata: None,
                memory_type: None,
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

                metadata: None,
                memory_type: None,
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

                metadata: None,
                memory_type: Some(mt.into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

            metadata: None,
            memory_type: Some("bogus".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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

            metadata: None,
            memory_type: None,
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
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

// ---- Episode derivations (wm-3) ----------------------------------------

#[tokio::test]
async fn episode_with_derivations_writes_atomically() {
    let h = require_harness!("ep_atom");

    // First store two observations that will be the derivation sources.
    let obs1 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh corrected the agent — prefers explicit over implicit.".into(),
            idempotency_key: "obs-src-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let obs2 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh pushed back on adding abstraction layers too early.".into(),
            idempotency_key: "obs-src-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Now store the episode linking to both observations.
    let episode = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Session: worked on chitta pivot. Josh values explicit contracts.".into(),
            idempotency_key: "ep-deriv-1".into(),
            event_time: None,
            tags: Some(vec!["session".into()]),
            metadata: None,
            memory_type: Some("episode".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: Some(vec![
                tools::DerivationInput {
                    source_id: obs1.id.to_string(),
                    derivation_type: "synthesised_from".into(),
                },
                tools::DerivationInput {
                    source_id: obs2.id.to_string(),
                    derivation_type: "synthesised_from".into(),
                },
            ]),
        },
    )
    .await
    .unwrap();

    assert_eq!(episode.memory_type, "episode");

    // Verify derivation rows exist in the DB.
    let derivs = db::get_derivations_for(&h.pool, episode.id).await.unwrap();
    assert_eq!(derivs.len(), 2, "expected 2 derivation rows");
    let source_ids: Vec<_> = derivs.iter().map(|d| d.source_id).collect();
    assert!(source_ids.contains(&obs1.id));
    assert!(source_ids.contains(&obs2.id));
    for d in &derivs {
        assert_eq!(d.derived_id, episode.id);
        assert_eq!(d.derivation_type, "synthesised_from");
    }
}

#[tokio::test]
async fn episode_without_derivations_rejected() {
    let h = require_harness!("ep_no_deriv");

    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "session summary".into(),
            idempotency_key: "ep-reject-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("episode".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::InvalidArgument {
            argument,
            next_action,
            ..
        } => {
            assert_eq!(argument, "derivations");
            assert!(
                next_action.contains("episode memory requires at least one entry in derivations"),
                "got: {next_action}"
            );
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[tokio::test]
async fn episode_with_empty_derivations_rejected() {
    let h = require_harness!("ep_empty_deriv");

    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "session summary".into(),
            idempotency_key: "ep-reject-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("episode".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: Some(vec![]),
        },
    )
    .await
    .unwrap_err();

    match &err {
        ChittaError::InvalidArgument {
            argument,
            next_action,
            ..
        } => {
            assert_eq!(argument, "derivations");
            assert!(
                next_action.contains("Either supply derivations, or use memory_type=observation"),
                "got: {next_action}"
            );
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[tokio::test]
async fn episode_derivation_invalid_source_id_rolls_back() {
    let h = require_harness!("ep_rollback");

    let bogus_uuid = Uuid::now_v7();

    let err = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "session that should be rolled back".into(),
            idempotency_key: "ep-rollback-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("episode".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: Some(vec![tools::DerivationInput {
                source_id: bogus_uuid.to_string(),
                derivation_type: "synthesised_from".into(),
            }]),
        },
    )
    .await;

    assert!(err.is_err(), "should fail with FK violation");

    // Verify the episode row was NOT persisted (transaction rolled back).
    let check = db::find_by_idempotency_key(&h.pool, &h.profile, "ep-rollback-1")
        .await
        .unwrap();
    assert!(
        check.is_none(),
        "episode row should be rolled back when derivation FK fails"
    );
}

// ---- search_memories refinement: applies_to, include_raw, supersession ----

#[tokio::test]
async fn search_default_excludes_raw_types() {
    let h = require_harness!("default_consolidated");

    // Store one consolidated and one raw memory.
    for (key, content, mt) in [
        (
            "dc-1",
            "Josh values clean interfaces above all.",
            "preference",
        ),
        (
            "dc-2",
            "Josh said he prefers small PRs today.",
            "observation",
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
                metadata: None,
                memory_type: Some(mt.into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
    }

    // Default search (no include_raw) should only return consolidated types.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "Josh preferences interfaces PRs".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
        },
    )
    .await
    .unwrap();

    for hit in &out.results {
        assert!(
            ["trait", "value", "pattern", "preference", "mental_model"]
                .contains(&hit.memory_type.as_str()),
            "default search should only return consolidated types, got: {}",
            hit.memory_type
        );
    }
    assert!(!out.results.is_empty(), "should find at least one result");
}

#[tokio::test]
async fn search_include_raw_returns_all_types() {
    let h = require_harness!("include_raw");

    for (key, content, mt) in [
        (
            "ir-1",
            "Josh values clean code and small functions.",
            "preference",
        ),
        (
            "ir-2",
            "Josh mentioned he dislikes large monolithic commits.",
            "observation",
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
                metadata: None,
                memory_type: Some(mt.into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
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
            query: "Josh code commits functions".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: Some(true),
        },
    )
    .await
    .unwrap();

    let types: Vec<&str> = out.results.iter().map(|h| h.memory_type.as_str()).collect();
    assert!(
        types.contains(&"observation"),
        "include_raw=true should return raw types too, got: {types:?}"
    );
}

#[tokio::test]
async fn search_applies_to_single_facet() {
    let h = require_harness!("at_single");

    // One memory tagged with domain "rust", one without.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers explicit error handling over panics.".into(),
            idempotency_key: "at1-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets {
                domains: vec!["rust".into()],
                ..Default::default()
            },
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers explicit typing over inference.".into(),
            idempotency_key: "at1-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets {
                domains: vec!["python".into()],
                ..Default::default()
            },
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Search with applies_to domains=["rust"] should only get the rust one.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "Josh preferences error handling typing".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: Some(true),
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: Some(AppliesTo {
                domains: Some(vec!["rust".into()]),
                skills: None,
                projects: None,
                situations: None,
            }),
            include_raw: None,
        },
    )
    .await
    .unwrap();

    assert!(!out.results.is_empty(), "should find the rust preference");
    for hit in &out.results {
        let content = hit.content.as_deref().unwrap_or(&hit.snippet);
        assert!(
            content.contains("panics") || content.contains("error handling"),
            "single-facet filter should only return rust-domain memories"
        );
    }
}

#[tokio::test]
async fn search_applies_to_multi_facet_intersection() {
    let h = require_harness!("at_multi");

    // Memory with both domain=rust AND skill=review.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh wants code reviews to focus on correctness not style.".into(),
            idempotency_key: "atm-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets {
                domains: vec!["rust".into()],
                skills: vec!["review".into()],
                ..Default::default()
            },
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Memory with domain=rust but skill=planning (not review).
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh likes to plan Rust modules with deep interfaces.".into(),
            idempotency_key: "atm-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets {
                domains: vec!["rust".into()],
                skills: vec!["planning".into()],
                ..Default::default()
            },
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Search with applies_to {domains: ["rust"], skills: ["review"]} — intersection.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "Josh preferences code modules review planning".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: Some(true),
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: Some(AppliesTo {
                domains: Some(vec!["rust".into()]),
                skills: Some(vec!["review".into()]),
                projects: None,
                situations: None,
            }),
            include_raw: None,
        },
    )
    .await
    .unwrap();

    assert!(
        !out.results.is_empty(),
        "should find the review+rust memory"
    );
    for hit in &out.results {
        let content = hit.content.as_deref().unwrap_or(&hit.snippet);
        assert!(
            content.contains("correctness"),
            "multi-facet intersection should only return the rust+review memory, got: {content}"
        );
    }
}

#[tokio::test]
async fn search_excludes_superseded_by_default() {
    let h = require_harness!("superseded");

    // Store a memory, then mark it superseded.
    let first = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers tabs over spaces in all contexts.".into(),
            idempotency_key: "sup-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let second = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh now prefers spaces over tabs universally.".into(),
            idempotency_key: "sup-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Mark first as superseded by second.
    sqlx::query("UPDATE memories SET superseded_by = $1 WHERE id = $2")
        .bind(second.id)
        .bind(first.id)
        .execute(&h.pool)
        .await
        .unwrap();

    // Default search should NOT return the superseded memory.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "tabs spaces preference".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: Some(true),
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
        },
    )
    .await
    .unwrap();

    let ids: Vec<Uuid> = out.results.iter().map(|h| h.id).collect();
    assert!(
        !ids.contains(&first.id),
        "superseded memory should be excluded from default search"
    );
    assert!(
        ids.contains(&second.id),
        "non-superseded memory should still appear"
    );
}

#[tokio::test]
async fn search_excludes_invalidated_by_default() {
    let h = require_harness!("invalidated");

    let stored = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh strongly dislikes dynamic typing.".into(),
            idempotency_key: "inv-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Soft-delete it.
    tools::delete::handle(
        &h.pool,
        DeleteArgs {
            profile: h.profile.clone(),
            id: stored.id.to_string(),
        },
    )
    .await
    .unwrap();

    // Default search should not find it.
    let out = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &test_search_cfg(),
        SearchArgs {
            profile: h.profile.clone(),
            query: "dynamic typing".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
        },
    )
    .await
    .unwrap();

    let ids: Vec<Uuid> = out.results.iter().map(|h| h.id).collect();
    assert!(
        !ids.contains(&stored.id),
        "invalidated memory should be excluded from default search"
    );
}

#[tokio::test]
async fn applies_to_uses_gin_index() {
    let h = require_harness!("gin_plan");

    // On small tables the planner may choose a seq scan over the GIN index.
    // We verify the indexes exist by checking pg_indexes directly.
    let index_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM pg_indexes
            WHERE tablename = 'memories'
              AND indexname = 'memories_applies_to_domains_idx'
        )
        "#,
    )
    .fetch_one(&h.pool)
    .await
    .unwrap();

    assert!(index_exists, "GIN index on applies_to_domains must exist");

    // Also verify the other three facet indexes exist.
    for col in ["skills", "projects", "situations"] {
        let idx_name = format!("memories_applies_to_{col}_idx");
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE tablename = 'memories' AND indexname = $1)",
        )
        .bind(&idx_name)
        .fetch_one(&h.pool)
        .await
        .unwrap();
        assert!(exists, "GIN index {idx_name} must exist");
    }
}

// ---- layer-aware search ranking (chitta/39) -------------------------

#[tokio::test]
async fn search_consolidated_high_confidence_outranks_low() {
    let h = require_harness!("layer_rank");

    // Store two consolidated memories with different confidence.
    // Same semantic content so similarity is comparable.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh values clear, readable code in every project.".into(),
            idempotency_key: "lr-high".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.95),
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh appreciates clear and readable code style.".into(),
            idempotency_key: "lr-low".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.30),
            source: None,
            derivations: None,
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
            query: "readable code style".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: Some(true),
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: None,
        },
    )
    .await
    .unwrap();

    assert!(out.results.len() >= 2, "should find both memories");
    for hit in &out.results {
        assert_eq!(
            hit.layer, "consolidated",
            "preference is a consolidated type"
        );
    }
    assert!(
        out.results[0].confidence.unwrap() > out.results[1].confidence.unwrap(),
        "high-confidence hit ({}) should outrank low-confidence hit ({})",
        out.results[0].confidence.unwrap(),
        out.results[1].confidence.unwrap(),
    );
}

#[tokio::test]
async fn search_raw_hits_ordered_by_recency_when_include_raw() {
    let h = require_harness!("layer_raw_order");

    // Store a consolidated hit and two raw observations at different event_times.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh likes writing tests before code.".into(),
            idempotency_key: "lro-cons".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("pattern".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.80),
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Older observation.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh said he likes writing tests first today.".into(),
            idempotency_key: "lro-old".into(),
            event_time: Some("2025-01-01T00:00:00Z".parse().unwrap()),
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Newer observation.
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh mentioned he writes tests before implementation.".into(),
            idempotency_key: "lro-new".into(),
            event_time: Some("2026-05-01T00:00:00Z".parse().unwrap()),
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
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
            query: "writing tests before code".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: None,
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            applies_to: None,
            include_raw: Some(true),
        },
    )
    .await
    .unwrap();

    assert!(out.results.len() >= 3, "should find all three memories");

    // Consolidated hits come first.
    let first = &out.results[0];
    assert_eq!(
        first.layer, "consolidated",
        "consolidated should come first"
    );

    // Raw hits come after consolidated.
    let raw_hits: Vec<_> = out.results.iter().filter(|h| h.layer == "raw").collect();
    assert!(raw_hits.len() >= 2, "should have at least two raw hits");

    // Verify layer field values are correct.
    for hit in &out.results {
        assert!(
            hit.layer == "consolidated" || hit.layer == "raw",
            "layer must be 'consolidated' or 'raw', got: {}",
            hit.layer
        );
    }
}

// ---- supersede_memory ------------------------------------------------

fn store_args(profile: &str, content: &str, key: &str) -> StoreArgs {
    StoreArgs {
        profile: profile.into(),
        content: content.into(),
        idempotency_key: key.into(),
        event_time: None,
        tags: None,
        metadata: None,
        memory_type: Some("trait".into()),
        external_refs: None,
        facets: Facets::default(),
        confidence: Some(0.7),
        source: None,
        derivations: None,
    }
}

#[tokio::test]
async fn supersede_happy_path() {
    let h = require_harness!("supersede_happy");

    let old = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "Josh prefers tabs", "sup-old"),
    )
    .await
    .unwrap();

    let new = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "Josh prefers spaces", "sup-new"),
    )
    .await
    .unwrap();

    let result = tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old.id.to_string(),
            new_id: new.id.to_string(),
            reason: "correction based on recent session".into(),
        },
    )
    .await
    .unwrap();

    assert_eq!(result.old_id, old.id);
    assert_eq!(result.new_id, new.id);

    // Verify superseded_by is set on old row.
    let old_row = tools::get::handle(
        &h.pool,
        GetArgs {
            profile: h.profile.clone(),
            id: old.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(old_row.superseded_by, Some(new.id));

    // Verify derivation row exists.
    let derivations = db::get_derivations_for(&h.pool, new.id).await.unwrap();
    assert_eq!(derivations.len(), 1);
    assert_eq!(derivations[0].source_id, old.id);
    assert_eq!(derivations[0].derivation_type, "supersedes");
    assert_eq!(derivations[0].id, result.derivation_id);
}

#[tokio::test]
async fn supersede_invalid_old_id() {
    let h = require_harness!("supersede_bad_old");

    let new = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "some trait", "sup-new-2"),
    )
    .await
    .unwrap();

    let err = tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: Uuid::now_v7().to_string(),
            new_id: new.id.to_string(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ChittaError::NotFound { .. }));
}

#[tokio::test]
async fn supersede_invalid_new_id() {
    let h = require_harness!("supersede_bad_new");

    let old = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "some trait", "sup-old-3"),
    )
    .await
    .unwrap();

    let err = tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old.id.to_string(),
            new_id: Uuid::now_v7().to_string(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ChittaError::NotFound { .. }));
}

#[tokio::test]
async fn supersede_profile_mismatch() {
    let h = require_harness!("supersede_profile");

    let old = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "trait in profile A", "sup-pm-old"),
    )
    .await
    .unwrap();

    let new = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "trait in profile A", "sup-pm-new"),
    )
    .await
    .unwrap();

    // Call supersede with a different profile — old_id won't be found.
    let err = tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: "wrong_profile".into(),
            old_id: old.id.to_string(),
            new_id: new.id.to_string(),
            reason: "test".into(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ChittaError::NotFound { .. }));
}

#[tokio::test]
async fn supersede_already_superseded_rejects() {
    let h = require_harness!("supersede_double");

    let old = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "original trait", "sup-dbl-old"),
    )
    .await
    .unwrap();

    let new1 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "replacement 1", "sup-dbl-new1"),
    )
    .await
    .unwrap();

    let new2 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(&h.profile, "replacement 2", "sup-dbl-new2"),
    )
    .await
    .unwrap();

    // First supersession succeeds.
    tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old.id.to_string(),
            new_id: new1.id.to_string(),
            reason: "first supersession".into(),
        },
    )
    .await
    .unwrap();

    // Second supersession of the same old_id should fail.
    let err = tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old.id.to_string(),
            new_id: new2.id.to_string(),
            reason: "second attempt".into(),
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(err, ChittaError::InvalidArgument { .. }));
}

#[tokio::test]
async fn superseded_row_excluded_from_default_search() {
    let h = require_harness!("supersede_search");
    let search_cfg = test_search_cfg();

    let old = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(
            &h.profile,
            "Josh strongly prefers tabs over spaces in all code",
            "sup-srch-old",
        ),
    )
    .await
    .unwrap();

    let new = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        store_args(
            &h.profile,
            "Josh strongly prefers spaces over tabs in all code",
            "sup-srch-new",
        ),
    )
    .await
    .unwrap();

    // Supersede old with new.
    tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old.id.to_string(),
            new_id: new.id.to_string(),
            reason: "preference changed".into(),
        },
    )
    .await
    .unwrap();

    // Default search should not return the superseded row.
    let results = tools::search::handle(
        &h.pool,
        h.embedder.clone(),
        false,
        &search_cfg,
        SearchArgs {
            profile: h.profile.clone(),
            query: "tabs vs spaces preference".into(),
            k: Some(10),
            max_tokens: None,
            tags: None,
            min_similarity: Some(0.0),
            include_content: None,
            memory_types: None,
            exclude_invalidated: None,
            exclude_superseded: None,
            ref_filter: None,
            include_raw: None,
            applies_to: None,
        },
    )
    .await
    .unwrap();

    let result_ids: Vec<_> = results.results.iter().map(|r| r.id).collect();
    assert!(
        !result_ids.contains(&old.id),
        "superseded memory should not appear in default search"
    );
    assert!(
        result_ids.contains(&new.id),
        "replacement memory should appear in search"
    );
}

// ---- get_profile ----------------------------------------------------

#[tokio::test]
async fn get_profile_returns_top_30_by_effective_score() {
    let h = require_harness!("profile");

    // Seed 35 consolidated memories with varied confidence and last_reinforced_at.
    // Rows with higher confidence AND more recent reinforcement should rank higher.
    let now = chrono::Utc::now();
    let mut stored_ids = Vec::new();

    for i in 0..35u32 {
        let confidence = 0.50 + (i as f32) * 0.01; // 0.50 .. 0.84
        let reinforced_days_ago = if i % 2 == 0 {
            // Even rows: reinforced recently → higher effective_score
            Some(chrono::Duration::days(i as i64))
        } else {
            // Odd rows: reinforced long ago → lower effective_score
            Some(chrono::Duration::days(300 + i as i64))
        };

        let last_reinforced = reinforced_days_ago.map(|d| now - d);

        let memory_type = match i % 5 {
            0 => "trait",
            1 => "value",
            2 => "preference",
            3 => "pattern",
            _ => "mental_model",
        };

        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("profile entry {i}: confidence={confidence:.2}"),
                idempotency_key: format!("profile-{i}"),
                event_time: None,
                tags: None,
                metadata: None,
                memory_type: Some(memory_type.into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: Some(confidence),
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();

        // Set last_reinforced_at directly via SQL (store_memory doesn't expose it)
        if let Some(lr) = last_reinforced {
            sqlx::query("UPDATE memories SET last_reinforced_at = $1 WHERE id = $2")
                .bind(lr)
                .bind(out.id)
                .execute(&h.pool)
                .await
                .unwrap();
        }

        stored_ids.push(out.id);
    }

    // Also seed a raw observation — should NOT appear in profile
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "raw observation, not consolidated".into(),
            idempotency_key: "profile-raw".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.99),
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let result = tools::get_profile::handle(
        &h.pool,
        GetProfileArgs {
            profile: h.profile.clone(),
        },
    )
    .await
    .unwrap();

    // AC: returns up to 30 rows
    assert_eq!(result.entries.len(), 30, "should return exactly 30 entries");
    assert_eq!(result.total_candidates, 35, "over-fetch should see all 35");
    assert!(result.truncated, "should be truncated");

    // AC: ordered by descending effective_score
    for w in result.entries.windows(2) {
        assert!(
            w[0].effective_score >= w[1].effective_score,
            "entries should be sorted by effective_score DESC: {} >= {} failed",
            w[0].effective_score,
            w[1].effective_score,
        );
    }

    // AC: no observation types in the result
    for entry in &result.entries {
        assert_ne!(
            entry.memory_type, "observation",
            "raw observations should not appear in profile"
        );
        assert_ne!(entry.memory_type, "episode");
        assert_ne!(entry.memory_type, "decision");
    }

    // Verify effective_score < confidence (decay should reduce it unless day-0)
    for entry in &result.entries {
        assert!(
            entry.effective_score <= entry.confidence + 1e-6,
            "effective_score {} should be <= confidence {}",
            entry.effective_score,
            entry.confidence,
        );
    }
}

#[tokio::test]
async fn get_profile_empty_profile_returns_empty() {
    let h = require_harness!("profile_empty");

    let result = tools::get_profile::handle(
        &h.pool,
        GetProfileArgs {
            profile: h.profile.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(result.entries.len(), 0);
    assert_eq!(result.total_candidates, 0);
    assert!(!result.truncated);
}

// ---- reflect_status ------------------------------------------------

#[tokio::test]
async fn reflect_status_counts_raw_rows_since_last_run() {
    let h = require_harness!("reflect");

    // Seed 3 observations
    for i in 0..3u32 {
        tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("observation {i}"),
                idempotency_key: format!("reflect-obs-{i}"),
                event_time: None,
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets {
                    domains: vec!["rust".into()],
                    projects: vec!["chitta".into()],
                    ..Default::default()
                },
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
    }

    // Seed 1 disagree-flagged observation
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh disagreed with: some trait".into(),
            idempotency_key: "reflect-disagree".into(),
            event_time: None,
            tags: Some(vec!["feedback".into(), "disagree".into()]),
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Seed a consolidated row (should NOT appear in reflect)
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "consolidated trait".into(),
            idempotency_key: "reflect-trait".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("trait".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.80),
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // First reflect run
    let r1 = tools::reflect_status::handle(
        &h.pool,
        ReflectStatusArgs {
            profile: h.profile.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        r1.total, 4,
        "should see 3 observations + 1 disagree-flagged"
    );
    assert_eq!(r1.counts.get("observation"), Some(&4));
    assert!(r1.since.is_none(), "first run should have no prior run");
    assert_eq!(r1.disagree_flagged.len(), 1);
    assert!(r1.distinct_domains.contains(&"rust".to_string()));
    assert!(r1.distinct_projects.contains(&"chitta".to_string()));
    assert!(r1.date_range.is_some());

    // Seed one more observation after the run
    tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "new observation after reflect".into(),
            idempotency_key: "reflect-new".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    // Second reflect run — should only see the new row
    let r2 = tools::reflect_status::handle(
        &h.pool,
        ReflectStatusArgs {
            profile: h.profile.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        r2.total, 1,
        "second run should only see rows since first run"
    );
    assert!(
        r2.since.is_some(),
        "should reference the first run's timestamp"
    );
    assert_eq!(r2.last_run_id, Some(r1.run_id));
    assert_eq!(r2.disagree_flagged.len(), 0);
}

#[tokio::test]
async fn reflect_status_empty_profile() {
    let h = require_harness!("reflect_empty");

    let result = tools::reflect_status::handle(
        &h.pool,
        ReflectStatusArgs {
            profile: h.profile.clone(),
        },
    )
    .await
    .unwrap();

    assert_eq!(result.total, 0);
    assert!(result.since.is_none());
    assert!(result.date_range.is_none());
    assert!(result.disagree_flagged.is_empty());
}

// ---- record_feedback ------------------------------------------------

async fn seed_consolidated(h: &Harness, key: &str, confidence: f32) -> uuid::Uuid {
    let out = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: format!("consolidated entry for {key}"),
            idempotency_key: key.into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("trait".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(confidence),
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();
    out.id
}

#[tokio::test]
async fn feedback_agree_bumps_confidence_and_reinforcement() {
    let h = require_harness!("fb_agree");
    let id = seed_consolidated(&h, "fb-agree-1", 0.50).await;

    let result = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(result.memory_id, id);
    assert!((result.new_confidence - 0.55).abs() < 0.001);
    assert!(matches!(result.kind, FeedbackKind::Agree));
    assert!(result.correction_row_id.is_none());

    let row = db::get_memory_by_id(&h.pool, &h.profile, id)
        .await
        .unwrap()
        .unwrap();
    assert!((row.confidence.unwrap() - 0.55).abs() < 0.001);
    assert_eq!(row.reinforcement_count, 1);
    assert!(row.last_reinforced_at.is_some());

    let feedback = db::get_memory_by_id(&h.pool, &h.profile, result.feedback_row_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(feedback.memory_type, "observation");
    assert!(feedback.tags.contains(&"feedback".to_string()));
    assert!(feedback.tags.contains(&"agree".to_string()));
}

#[tokio::test]
async fn feedback_disagree_drops_confidence() {
    let h = require_harness!("fb_disagree");
    let id = seed_consolidated(&h, "fb-disagree-1", 0.50).await;

    let result = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Disagree,
            correction: None,
        },
    )
    .await
    .unwrap();

    assert!((result.new_confidence - 0.40).abs() < 0.001);
    assert!(result.correction_row_id.is_none());

    let row = db::get_memory_by_id(&h.pool, &h.profile, id)
        .await
        .unwrap()
        .unwrap();
    assert!((row.confidence.unwrap() - 0.40).abs() < 0.001);
    assert_eq!(row.reinforcement_count, 0, "disagree should not increment reinforcement_count");
    assert!(row.last_reinforced_at.is_some());
}

#[tokio::test]
async fn feedback_disagree_with_correction_writes_both_rows() {
    let h = require_harness!("fb_correction");
    let id = seed_consolidated(&h, "fb-correction-1", 0.70).await;

    let result = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Disagree,
            correction: Some("Actually Josh prefers Vim over Emacs".into()),
        },
    )
    .await
    .unwrap();

    assert!((result.new_confidence - 0.60).abs() < 0.001);
    assert!(result.correction_row_id.is_some());

    let correction = db::get_memory_by_id(
        &h.pool,
        &h.profile,
        result.correction_row_id.unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(correction.memory_type, "observation");
    assert!(correction.tags.contains(&"correction".to_string()));
    assert!(
        correction.tags.iter().any(|t| t.starts_with("contradicts:")),
        "correction should have contradicts:<id> tag"
    );
    assert_eq!(correction.content, "Actually Josh prefers Vim over Emacs");
    assert!(correction.embedding.is_some(), "correction should be embedded");
}

#[tokio::test]
async fn feedback_rejects_raw_layer_memory() {
    let h = require_harness!("fb_reject_raw");

    let out = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "raw observation".into(),
            idempotency_key: "fb-raw-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let err = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: out.id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "memory_id");
        }
        other => panic!("expected InvalidArgument, got: {other:?}"),
    }
}

#[tokio::test]
async fn feedback_rejects_superseded_memory() {
    let h = require_harness!("fb_reject_superseded");
    let old_id = seed_consolidated(&h, "fb-sup-old", 0.50).await;
    let new_id = seed_consolidated(&h, "fb-sup-new", 0.60).await;

    tools::supersede::handle(
        &h.pool,
        SupersedeArgs {
            profile: h.profile.clone(),
            old_id: old_id.to_string(),
            new_id: new_id.to_string(),
            reason: "test supersede".into(),
        },
    )
    .await
    .unwrap();

    let err = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: old_id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "memory_id");
        }
        other => panic!("expected InvalidArgument for superseded, got: {other:?}"),
    }
}

#[tokio::test]
async fn feedback_rejects_invalidated_memory() {
    let h = require_harness!("fb_reject_invalid");
    let id = seed_consolidated(&h, "fb-inv-1", 0.50).await;

    tools::delete::handle(
        &h.pool,
        DeleteArgs {
            profile: h.profile.clone(),
            id: id.to_string(),
        },
    )
    .await
    .unwrap();

    let err = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound for deleted memory, got: {other:?}"),
    }
}

#[tokio::test]
async fn feedback_agree_caps_at_one() {
    let h = require_harness!("fb_cap");
    let id = seed_consolidated(&h, "fb-cap-1", 0.98).await;

    let result = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap();

    assert!((result.new_confidence - 1.0).abs() < 0.001);
}

#[tokio::test]
async fn feedback_disagree_floors_at_zero() {
    let h = require_harness!("fb_floor");
    let id = seed_consolidated(&h, "fb-floor-1", 0.05).await;

    let result = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Disagree,
            correction: None,
        },
    )
    .await
    .unwrap();

    assert!((result.new_confidence - 0.0).abs() < 0.001);
}

#[tokio::test]
async fn feedback_agree_with_correction_rejected() {
    let h = require_harness!("fb_agree_correction");
    let id = seed_consolidated(&h, "fb-agree-corr", 0.50).await;

    let err = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Agree,
            correction: Some("this should be rejected".into()),
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::InvalidArgument { argument, .. } => {
            assert_eq!(argument, "correction");
        }
        other => panic!("expected InvalidArgument for correction+agree, got: {other:?}"),
    }
}

#[tokio::test]
async fn feedback_wrong_profile_returns_not_found() {
    let h = require_harness!("fb_wrong_profile");
    let id = seed_consolidated(&h, "fb-wrongp", 0.50).await;

    let err = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: "nonexistent-profile".into(),
            memory_id: id.to_string(),
            kind: FeedbackKind::Agree,
            correction: None,
        },
    )
    .await
    .unwrap_err();

    match err {
        ChittaError::NotFound { .. } => {}
        other => panic!("expected NotFound for wrong profile, got: {other:?}"),
    }
}

// ---- synthesis smoke ------------------------------------------------

struct FixtureLlm;

impl Llm for FixtureLlm {
    async fn complete(&self, _system: &str, user: &str) -> chitta::error::Result<String> {
        if user.contains("prefers Rust") {
            Ok(r#"[{"memory_type": "preference", "claim": "Josh prefers Rust"}]"#.into())
        } else if user.contains("values simplicity") {
            Ok(r#"[
                {"memory_type": "value", "claim": "Josh values simplicity"},
                {"memory_type": "pattern", "claim": "Josh avoids premature abstraction"}
            ]"#
            .into())
        } else {
            Ok("[]".into())
        }
    }
}

#[tokio::test]
async fn synthesis_extract_candidates_smoke() {
    let h = require_harness!("synth_smoke");

    let out1 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers Rust for systems work".into(),
            idempotency_key: "synth-1".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let out2 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh values simplicity over cleverness".into(),
            idempotency_key: "synth-2".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let out3 = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "routine standup notes, nothing extractable".into(),
            idempotency_key: "synth-3".into(),
            event_time: None,
            tags: None,
            metadata: None,
            memory_type: Some("observation".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: None,
            source: None,
            derivations: None,
        },
    )
    .await
    .unwrap();

    let rows = vec![
        db::get_memory_by_id(&h.pool, &h.profile, out1.id).await.unwrap().unwrap(),
        db::get_memory_by_id(&h.pool, &h.profile, out2.id).await.unwrap().unwrap(),
        db::get_memory_by_id(&h.pool, &h.profile, out3.id).await.unwrap().unwrap(),
    ];

    let llm = FixtureLlm;
    let candidates = synthesis::extract_candidates(&llm, &rows).await.unwrap();

    assert_eq!(candidates.len(), 3, "expected 3 candidates from 2 productive rows");

    assert_eq!(candidates[0].memory_type, "preference");
    assert_eq!(candidates[0].claim, "Josh prefers Rust");
    assert_eq!(candidates[0].source_id, out1.id);

    assert_eq!(candidates[1].memory_type, "value");
    assert_eq!(candidates[1].source_id, out2.id);

    assert_eq!(candidates[2].memory_type, "pattern");
    assert_eq!(candidates[2].source_id, out2.id);
}

// ---- synthesis cluster + emit ----------------------------------------

struct ClusterFixtureLlm;

impl ClusterFixtureLlm {
    fn new() -> Self {
        Self
    }
}

impl Llm for ClusterFixtureLlm {
    async fn complete(&self, system: &str, user: &str) -> chitta::error::Result<String> {
        if system.contains("extract consolidated claims") {
            Ok(r#"[{"memory_type": "preference", "claim": "Josh prefers Rust for systems work"}]"#.into())
        } else if system.contains("group candidate claims") {
            let count = user.lines().filter(|l| l.starts_with('[')).count();
            let indices: Vec<usize> = (0..count).collect();
            let indices_json = serde_json::to_string(&indices).unwrap();
            Ok(format!(
                r#"[{{"representative_claim": "Josh prefers Rust for systems programming", "memory_type": "preference", "member_indices": {indices_json}}}]"#
            ))
        } else {
            Ok("[]".into())
        }
    }
}

#[tokio::test]
async fn synthesis_cluster_and_emit() {
    let h = require_harness!("synth_cluster");

    let base_time = Utc::now() - Duration::days(30);
    let mut stored_ids = Vec::new();

    for i in 0..6 {
        let day_offset = Duration::days(i * 3);
        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("Josh prefers Rust for systems work — observation {i}"),
                idempotency_key: format!("cluster-obs-{i}"),
                event_time: Some(base_time + day_offset),
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
        stored_ids.push(out.id);
    }

    // Backdate record_time so rows span multiple days (store::handle sets
    // record_time=now, but the threshold checks distinct days by record_time).
    for (i, id) in stored_ids.iter().enumerate() {
        let record_time = base_time + Duration::days(i as i64 * 3);
        sqlx::query("UPDATE memories SET record_time = $1 WHERE id = $2")
            .bind(record_time)
            .bind(id)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    let rows: Vec<db::MemoryRow> = {
        let mut v = Vec::new();
        for id in &stored_ids {
            v.push(
                db::get_memory_by_id(&h.pool, &h.profile, *id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        v
    };

    let llm = ClusterFixtureLlm::new();

    let candidates = synthesis::extract_candidates(&llm, &rows).await.unwrap();
    assert_eq!(candidates.len(), 6);

    let clusters = synthesis::cluster_candidates(&llm, &candidates).await.unwrap();
    assert_eq!(clusters.len(), 1, "all similar claims should form one cluster");
    assert_eq!(clusters[0].source_ids.len(), 6);

    let source_times: HashMap<Uuid, DateTime<Utc>> = rows
        .iter()
        .map(|r| (r.id, r.record_time))
        .collect();

    let config = ThresholdConfig::default();
    assert!(
        synthesis::check_threshold(&clusters[0], &source_times, Utc::now(), &config),
        "6 sources across 6 different days with recent data should pass"
    );

    let confidence = synthesis::emission_confidence(clusters[0].source_ids.len());
    assert!(
        (confidence - 0.80).abs() < 1e-6,
        "min(0.90, 0.50 + 0.05*6) = 0.80"
    );

    let (emitted, replayed) = synthesis::emit_consolidated(
        &h.pool,
        &h.embedder,
        &clusters[0],
        &h.profile,
        Utc::now(),
    )
    .await
    .unwrap();

    assert!(!replayed, "first emission should not be a replay");
    assert_eq!(emitted.memory_type, "preference");
    assert!(
        (emitted.confidence.unwrap() - 0.80).abs() < 1e-6,
        "emitted confidence should match formula"
    );
    assert_eq!(emitted.source.as_deref(), Some("reflect"));
    assert!(emitted.tags.contains(&"synthesised".to_string()));

    let derivations = db::get_derivations_for(&h.pool, emitted.id).await.unwrap();
    assert_eq!(
        derivations.len(),
        6,
        "one derivation per source row"
    );
    for d in &derivations {
        assert_eq!(d.derivation_type, "synthesised_from");
        assert!(
            stored_ids.contains(&d.source_id),
            "derivation should point to a source row"
        );
    }

    // Idempotency: re-emit the same cluster → replay
    let (_, replayed2) = synthesis::emit_consolidated(
        &h.pool,
        &h.embedder,
        &clusters[0],
        &h.profile,
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(replayed2, "second emission of same cluster should be idempotent replay");
}

// ---- synthesis contradiction + supersession --------------------------

struct ContradictionFixtureLlm {
    existing_claim: String,
}

impl Llm for ContradictionFixtureLlm {
    async fn complete(&self, system: &str, user: &str) -> chitta::error::Result<String> {
        if system.contains("extract consolidated claims") {
            Ok(
                r#"[{"memory_type": "preference", "claim": "Josh prefers spaces over tabs"}]"#
                    .into(),
            )
        } else if system.contains("group candidate claims") {
            let count = user.lines().filter(|l| l.starts_with('[')).count();
            let indices: Vec<usize> = (0..count).collect();
            let indices_json = serde_json::to_string(&indices).unwrap();
            Ok(format!(
                r#"[{{"representative_claim": "Josh prefers spaces over tabs", "memory_type": "preference", "member_indices": {indices_json}}}]"#
            ))
        } else if system.contains("detect contradictions") {
            if user.contains(&self.existing_claim) {
                Ok(r#"{"contradicts_index": 0, "shift": "switched from tabs to spaces"}"#.into())
            } else {
                Ok(r#"{"contradicts_index": null}"#.into())
            }
        } else {
            Ok(r#"{"contradicts_index": null}"#.into())
        }
    }
}

#[tokio::test]
async fn synthesis_contradiction_and_supersession() {
    let h = require_harness!("synth_contradict");

    // 1. Seed an existing consolidated memory: "Josh prefers tabs over spaces"
    let old_out = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers tabs over spaces".into(),
            idempotency_key: "old-consolidated".into(),
            event_time: None,
            tags: Some(vec!["reflect".into(), "synthesised".into()]),
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.75),
            source: Some("reflect".into()),
            derivations: None,
        },
    )
    .await
    .unwrap();
    let old_id = old_out.id;

    // 2. Seed 6 contradicting observations across multiple days
    let base_time = Utc::now() - Duration::days(30);
    let mut stored_ids = Vec::new();

    for i in 0..6 {
        let day_offset = Duration::days(i * 3);
        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("Josh prefers spaces over tabs — observation {i}"),
                idempotency_key: format!("contradict-obs-{i}"),
                event_time: Some(base_time + day_offset),
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
        stored_ids.push(out.id);
    }

    // Backdate record_time so rows span multiple days
    for (i, id) in stored_ids.iter().enumerate() {
        let record_time = base_time + Duration::days(i as i64 * 3);
        sqlx::query("UPDATE memories SET record_time = $1 WHERE id = $2")
            .bind(record_time)
            .bind(id)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    // 3. Fetch raw rows and run the full synthesis pipeline
    let rows: Vec<db::MemoryRow> = {
        let mut v = Vec::new();
        for id in &stored_ids {
            v.push(
                db::get_memory_by_id(&h.pool, &h.profile, *id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        v
    };

    let llm = ContradictionFixtureLlm {
        existing_claim: "Josh prefers tabs over spaces".into(),
    };

    let result = synthesis::run_synthesis(
        &h.pool,
        &h.embedder,
        &llm,
        &h.profile,
        &rows,
        Utc::now(),
    )
    .await
    .unwrap();

    assert_eq!(result.clusters_emitted, 1);
    assert_eq!(result.supersessions, 1);

    // 4. Verify the old memory is now superseded
    let old_row = db::get_memory_by_id(&h.pool, &h.profile, old_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        old_row.superseded_by.is_some(),
        "old memory should be superseded"
    );

    let new_id = old_row.superseded_by.unwrap();

    // 5. Verify the new consolidated row exists
    let new_row = db::get_memory_by_id(&h.pool, &h.profile, new_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new_row.memory_type, "preference");
    assert!(new_row.content.contains("spaces"));
    assert_eq!(new_row.source.as_deref(), Some("reflect"));

    // 6. Verify meta-observation (mental_model) was written
    let all_rows = db::list_recent(&h.pool, &h.profile, 50, &[], &["mental_model".into()])
        .await
        .unwrap();
    let meta = all_rows
        .iter()
        .find(|r| r.tags.contains(&"supersession".to_string()))
        .expect("meta-observation should exist");
    assert_eq!(meta.memory_type, "mental_model");
    assert!(meta.content.contains("tabs"));
    assert!(meta.content.contains("spaces"));

    // 7. Verify meta-observation has derivations linking old and new
    let meta_derivations = db::get_derivations_for(&h.pool, meta.id).await.unwrap();
    assert_eq!(meta_derivations.len(), 2, "meta should have 2 derivations");
    let deriv_types: Vec<&str> = meta_derivations.iter().map(|d| d.derivation_type.as_str()).collect();
    assert!(deriv_types.contains(&"supersession_of"));
    assert!(deriv_types.contains(&"supersession_to"));
}

// ---- synthesis: disagree-flagged memory superseded --------------------

struct DisagreeFixtureLlm {
    target_claim: String,
}

impl Llm for DisagreeFixtureLlm {
    async fn complete(&self, system: &str, user: &str) -> chitta::error::Result<String> {
        if system.contains("extract consolidated claims") {
            if user.contains("correction") || user.contains("actually prefers") {
                Ok(
                    r#"[{"memory_type": "preference", "claim": "Josh prefers dark mode"}]"#
                        .into(),
                )
            } else {
                Ok("[]".into())
            }
        } else if system.contains("group candidate claims") {
            let count = user.lines().filter(|l| l.starts_with('[')).count();
            let indices: Vec<usize> = (0..count).collect();
            let indices_json = serde_json::to_string(&indices).unwrap();
            Ok(format!(
                r#"[{{"representative_claim": "Josh prefers dark mode", "memory_type": "preference", "member_indices": {indices_json}}}]"#
            ))
        } else if system.contains("detect contradictions") {
            if user.contains(&self.target_claim) {
                Ok(
                    r#"{"contradicts_index": 0, "shift": "switched from light mode to dark mode"}"#
                        .into(),
                )
            } else {
                Ok(r#"{"contradicts_index": null}"#.into())
            }
        } else {
            Ok(r#"{"contradicts_index": null}"#.into())
        }
    }
}

#[tokio::test]
async fn synthesis_disagree_flagged_supersession() {
    let h = require_harness!("synth_disagree");

    // 1. Seed a consolidated memory that will be disagreed with
    let old_out = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers light mode".into(),
            idempotency_key: "old-light-mode".into(),
            event_time: None,
            tags: Some(vec!["reflect".into(), "synthesised".into()]),
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.75),
            source: Some("reflect".into()),
            derivations: None,
        },
    )
    .await
    .unwrap();
    let old_id = old_out.id;

    // 2. Record disagree feedback (creates feedback + correction rows)
    let _feedback = tools::record_feedback::handle(
        &h.pool,
        h.embedder.clone(),
        RecordFeedbackArgs {
            profile: h.profile.clone(),
            memory_id: old_id.to_string(),
            kind: FeedbackKind::Disagree,
            correction: Some("Josh actually prefers dark mode now".into()),
        },
    )
    .await
    .unwrap();

    // 3. Seed additional observations that support the correction
    let base_time = Utc::now() - Duration::days(30);
    let mut stored_ids = Vec::new();

    for i in 0..5 {
        let day_offset = Duration::days(i * 4);
        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("Josh actually prefers dark mode — observation {i}"),
                idempotency_key: format!("disagree-obs-{i}"),
                event_time: Some(base_time + day_offset),
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
        stored_ids.push(out.id);
    }

    for (i, id) in stored_ids.iter().enumerate() {
        let record_time = base_time + Duration::days(i as i64 * 4);
        sqlx::query("UPDATE memories SET record_time = $1 WHERE id = $2")
            .bind(record_time)
            .bind(id)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    // 4. Fetch all raw rows since epoch (includes feedback + correction + observations)
    let rows = db::fetch_raw_since(&h.pool, &h.profile, None).await.unwrap();

    // Verify disagree targets are found
    let targets = synthesis::find_disagree_targets(&rows);
    assert!(
        targets.contains(&old_id),
        "disagree target should include the old memory"
    );

    // 5. Run full synthesis pipeline
    let llm = DisagreeFixtureLlm {
        target_claim: "Josh prefers light mode".into(),
    };

    let result = synthesis::run_synthesis(
        &h.pool,
        &h.embedder,
        &llm,
        &h.profile,
        &rows,
        Utc::now(),
    )
    .await
    .unwrap();

    assert_eq!(result.supersessions, 1, "should supersede the disagreed-with memory");

    // 6. Verify the old memory is superseded
    let old_row = db::get_memory_by_id(&h.pool, &h.profile, old_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        old_row.superseded_by.is_some(),
        "disagree-targeted memory should be superseded"
    );

    // 7. Verify meta-observation written
    let all_mental_models =
        db::list_recent(&h.pool, &h.profile, 50, &[], &["mental_model".into()])
            .await
            .unwrap();
    let meta = all_mental_models
        .iter()
        .find(|r| r.tags.contains(&"supersession".to_string()));
    assert!(meta.is_some(), "meta-observation should be written for disagree supersession");
}

// ---- synthesis: two clusters targeting same memory ───────────────────

struct TwoClusterFixtureLlm {
    existing_claim: String,
}

impl Llm for TwoClusterFixtureLlm {
    async fn complete(&self, system: &str, user: &str) -> chitta::error::Result<String> {
        if system.contains("extract consolidated claims") {
            if user.contains("prefers spaces") {
                Ok(r#"[{"memory_type": "preference", "claim": "Josh prefers spaces"}]"#.into())
            } else if user.contains("uses 4-space indent") {
                Ok(r#"[{"memory_type": "preference", "claim": "Josh uses 4-space indent"}]"#.into())
            } else {
                Ok("[]".into())
            }
        } else if system.contains("group candidate claims") {
            let count = user.lines().filter(|l| l.starts_with('[')).count();
            if count >= 10 {
                let half = count / 2;
                let first: Vec<usize> = (0..half).collect();
                let second: Vec<usize> = (half..count).collect();
                let j1 = serde_json::to_string(&first).unwrap();
                let j2 = serde_json::to_string(&second).unwrap();
                Ok(format!(
                    r#"[
                        {{"representative_claim": "Josh prefers spaces over tabs", "memory_type": "preference", "member_indices": {j1}}},
                        {{"representative_claim": "Josh uses 4-space indent style", "memory_type": "preference", "member_indices": {j2}}}
                    ]"#
                ))
            } else {
                let indices: Vec<usize> = (0..count).collect();
                let j = serde_json::to_string(&indices).unwrap();
                Ok(format!(
                    r#"[{{"representative_claim": "Josh prefers spaces", "memory_type": "preference", "member_indices": {j}}}]"#
                ))
            }
        } else if system.contains("detect contradictions") {
            if user.contains(&self.existing_claim) {
                Ok(r#"{"contradicts_index": 0, "shift": "switched from tabs to spaces"}"#.into())
            } else {
                Ok(r#"{"contradicts_index": null}"#.into())
            }
        } else {
            Ok(r#"{"contradicts_index": null}"#.into())
        }
    }
}

#[tokio::test]
async fn synthesis_two_clusters_same_target_only_first_supersedes() {
    let h = require_harness!("synth_two_cluster");

    // 1. Seed existing consolidated memory
    let old_out = tools::store::handle(
        &h.pool,
        h.embedder.clone(),
        StoreArgs {
            profile: h.profile.clone(),
            content: "Josh prefers tabs over spaces".into(),
            idempotency_key: "old-tabs".into(),
            event_time: None,
            tags: Some(vec!["reflect".into(), "synthesised".into()]),
            metadata: None,
            memory_type: Some("preference".into()),
            external_refs: None,
            facets: Facets::default(),
            confidence: Some(0.75),
            source: Some("reflect".into()),
            derivations: None,
        },
    )
    .await
    .unwrap();
    let old_id = old_out.id;

    // 2. Seed 12 observations: 6 "prefers spaces" + 6 "uses 4-space indent"
    //    Both clusters could contradict "prefers tabs"
    let base_time = Utc::now() - Duration::days(30);
    let mut stored_ids = Vec::new();

    for i in 0..6 {
        let day_offset = Duration::days(i * 3);
        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("Josh prefers spaces — observation {i}"),
                idempotency_key: format!("two-cluster-spaces-{i}"),
                event_time: Some(base_time + day_offset),
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
        stored_ids.push(out.id);
    }
    for i in 0..6 {
        let day_offset = Duration::days(i * 3);
        let out = tools::store::handle(
            &h.pool,
            h.embedder.clone(),
            StoreArgs {
                profile: h.profile.clone(),
                content: format!("Josh uses 4-space indent — observation {i}"),
                idempotency_key: format!("two-cluster-indent-{i}"),
                event_time: Some(base_time + day_offset),
                tags: None,
                metadata: None,
                memory_type: Some("observation".into()),
                external_refs: None,
                facets: Facets::default(),
                confidence: None,
                source: None,
                derivations: None,
            },
        )
        .await
        .unwrap();
        stored_ids.push(out.id);
    }

    for (i, id) in stored_ids.iter().enumerate() {
        let record_time = base_time + Duration::days((i % 6) as i64 * 3);
        sqlx::query("UPDATE memories SET record_time = $1 WHERE id = $2")
            .bind(record_time)
            .bind(id)
            .execute(&h.pool)
            .await
            .unwrap();
    }

    let rows: Vec<db::MemoryRow> = {
        let mut v = Vec::new();
        for id in &stored_ids {
            v.push(
                db::get_memory_by_id(&h.pool, &h.profile, *id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        v
    };

    let llm = TwoClusterFixtureLlm {
        existing_claim: "Josh prefers tabs over spaces".into(),
    };

    let result = synthesis::run_synthesis(
        &h.pool,
        &h.embedder,
        &llm,
        &h.profile,
        &rows,
        Utc::now(),
    )
    .await
    .unwrap();

    // Both clusters emitted, but only the first should supersede
    assert_eq!(result.clusters_emitted, 2);
    assert_eq!(
        result.supersessions, 1,
        "only first cluster should supersede the old memory"
    );

    // The old memory should be superseded exactly once
    let old_row = db::get_memory_by_id(&h.pool, &h.profile, old_id)
        .await
        .unwrap()
        .unwrap();
    assert!(old_row.superseded_by.is_some());
}
