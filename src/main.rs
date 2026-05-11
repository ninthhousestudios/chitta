//! chitta binary entrypoint. See `docs/starting-shape.md` for scope.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use chitta::{
    config::{Config, chitta_home},
    db,
    embedding::Embedder,
    ingest,
    llm::ClaudeCliLlm,
    mcp::ChittaServer,
};

/// chitta: agent-native persistent memory MCP server.
#[derive(Debug, Parser)]
#[command(name = "chitta", version, about)]
struct Cli {
    /// Run as a Streamable HTTP server instead of stdio.
    #[arg(long)]
    http: bool,

    /// HTTP listen address (used with --http). Overrides CHITTA_HTTP_ADDR.
    #[arg(long)]
    http_addr: Option<String>,

    /// HTTP listen port (used with --http). Overrides CHITTA_HTTP_PORT.
    #[arg(long)]
    http_port: Option<u16>,

    /// Path to a file containing the bearer token for HTTP auth (required with --http).
    #[arg(long)]
    auth_token_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run chitta as a stdio MCP server (default when no subcommand given).
    Serve,
    /// Re-run logged queries against current DB state for regression detection.
    Replay {
        /// Filter to a specific memory profile.
        #[arg(long)]
        profile: Option<String>,
        /// Maximum number of query_log entries to replay.
        #[arg(long, default_value = "100")]
        limit: i64,
    },
    /// Backfill sparse embeddings for rows that have none.
    Backfill {
        /// Batch size for processing.
        #[arg(long, default_value = "100")]
        batch_size: i64,
    },
    /// Run working-model synthesis.
    Reflect {
        /// Profile to synthesize.
        #[arg(long, default_value = "josh")]
        profile: String,
        /// Model override.
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        /// Use Anthropic API instead of CLI subscription (reads ANTHROPIC_API_KEY from env).
        #[arg(long)]
        api_key: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env from ~/.chitta/.env first, then CWD as fallback.
    let _ = dotenvy::from_path(chitta_home().join(".env"));
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Replay { profile, limit }) => return run_replay(profile, limit).await,
        Some(Commands::Backfill { batch_size }) => return run_backfill(batch_size).await,
        Some(Commands::Reflect {
            profile,
            model,
            api_key,
        }) => return run_reflect(profile, model, api_key).await,
        Some(Commands::Serve) | None => {}
    }

    let cfg = Config::from_env().context("loading configuration from environment")?;

    // Logs go to stderr so stdout stays reserved for the MCP frame stream.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&cfg.log_level).unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        model_path = ?cfg.model_path,
        "starting chitta"
    );

    let pool = db::connect(&cfg).await.context("connecting to database")?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let query_log_enabled = if cfg.query_log {
        match sqlx::query("SELECT 1 FROM query_log LIMIT 0")
            .execute(&pool)
            .await
        {
            Ok(_) => {
                tracing::info!("query_log enabled");
                true
            }
            Err(sqlx::Error::Database(e)) if e.message().contains("does not exist") => {
                tracing::warn!(
                    "query_log table not found — run migration 0002 to enable search logging"
                );
                false
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e)
                    .context("query_log probe failed due to a database error — not starting with ambiguous state"));
            }
        }
    } else {
        false
    };

    let embedder = Embedder::load(
        &cfg.model_file(),
        &cfg.tokenizer_file(),
        cfg.embedder_pool_size,
        cfg.sparse_threshold,
    )
    .context("loading embedding model")?;

    if cli.http {
        serve_http(cli, cfg, pool, embedder, query_log_enabled).await
    } else {
        serve_stdio(pool, embedder, query_log_enabled, cfg.search).await
    }
}

/// Stdio transport — the original v0.0.1 path.
async fn serve_stdio(
    pool: sqlx::PgPool,
    embedder: Arc<Embedder>,
    query_log_enabled: bool,
    search_cfg: chitta::config::SearchConfig,
) -> Result<()> {
    let server = ChittaServer::new(pool, Arc::clone(&embedder), query_log_enabled, search_cfg);
    let (stdin, stdout) = stdio();

    let service = server
        .serve((stdin, stdout))
        .await
        .context("starting MCP service over stdio")?;

    tokio::select! {
        res = service.waiting() => {
            res.context("MCP service terminated with error")?;
        }
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received; exiting");
        }
    }

    Ok(())
}

async fn run_replay(profile: Option<String>, limit: i64) -> Result<()> {
    let cfg = Config::from_env().context("loading configuration from environment")?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn"))
        .with_writer(std::io::stderr)
        .init();

    let pool = db::connect(&cfg).await.context("connecting to database")?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let entries = db::read_query_log(&pool, profile.as_deref(), limit)
        .await
        .context("reading query_log")?;

    if entries.is_empty() {
        println!("No query_log entries found.");
        return Ok(());
    }

    println!("Replay Results ({} queries):", entries.len());
    println!(
        "┌─────┬──────────┬──────────────────────────────────────────────────────┬─────────┬──────────┬─────────┐"
    );
    println!(
        "│ {:<3} │ {:<8} │ {:<52} │ {:<7} │ {:<8} │ {:<7} │",
        "#", "Profile", "Query (50ch)", "Overlap", "New", "Dropped"
    );
    println!(
        "├─────┼──────────┼──────────────────────────────────────────────────────┼─────────┼──────────┼─────────┤"
    );

    let mut total_overlap: f64 = 0.0;

    for (idx, entry) in entries.iter().enumerate() {
        let (new_hits, _total) = db::search_by_embedding(
            &pool,
            &db::SearchParams {
                profile: &entry.profile,
                query: &entry.embedding,
                k: entry.k as i64,
                tags: &entry.tags,
                memory_types: &entry.memory_types,
                min_similarity: entry.min_similarity,
                recency_weight: 0.0,
                recency_half_life_days: 30.0,
                exclude_invalidated: true,
                exclude_superseded: false,
                ref_filter_json: None,
                facets: &chitta::facets::Facets::default(),
            },
        )
        .await
        .context("re-running search")?;

        let logged_ids: HashSet<Uuid> = entry.result_ids.iter().copied().collect();
        let new_ids: HashSet<Uuid> = new_hits.iter().map(|h| h.id).collect();

        let intersection = logged_ids.intersection(&new_ids).count();
        let union = logged_ids.union(&new_ids).count();

        let overlap_pct = if union == 0 {
            100.0_f64
        } else {
            intersection as f64 / union as f64 * 100.0
        };

        let new_count = new_ids.difference(&logged_ids).count();
        let dropped_count = logged_ids.difference(&new_ids).count();

        total_overlap += overlap_pct;

        let query_display: String = entry.query_text.chars().take(50).collect();
        let query_display = if entry.query_text.chars().count() > 50 {
            format!("{query_display}...")
        } else {
            query_display
        };

        let profile_display: String = entry.profile.chars().take(8).collect();

        println!(
            "│ {:<3} │ {:<8} │ {:<52} │ {:>6.0}% │ {:<8} │ {:<7} │",
            idx + 1,
            profile_display,
            query_display,
            overlap_pct,
            new_count,
            dropped_count,
        );
    }

    println!(
        "└─────┴──────────┴──────────────────────────────────────────────────────┴─────────┴──────────┴─────────┘"
    );

    let avg_overlap = total_overlap / entries.len() as f64;
    println!();
    println!(
        "Summary: {:.0}% average overlap across {} queries",
        avg_overlap,
        entries.len()
    );

    Ok(())
}

async fn run_backfill(batch_size: i64) -> Result<()> {
    let cfg = Config::from_env().context("loading configuration from environment")?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .init();

    let pool = db::connect(&cfg).await.context("connecting to database")?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let embedder = Embedder::load(
        &cfg.model_file(),
        &cfg.tokenizer_file(),
        cfg.embedder_pool_size,
        cfg.sparse_threshold,
    )
    .context("loading embedding model")?;

    let concurrency = embedder.pool_size();
    let mut total = 0u64;
    loop {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            r#"
            SELECT id, content FROM memories
            WHERE sparse_embedding IS NULL
            ORDER BY id
            LIMIT $1
            "#,
        )
        .bind(batch_size)
        .fetch_all(&pool)
        .await
        .context("fetching rows to backfill")?;

        if rows.is_empty() {
            break;
        }

        let batch_len = rows.len();
        let mut join_set = tokio::task::JoinSet::new();
        for (id, content) in rows {
            let embedder = Arc::clone(&embedder);
            let pool = pool.clone();
            join_set.spawn(async move {
                let embed_out = embedder
                    .embed_full(&content, "backfill")
                    .await
                    .context("embedding for backfill")?;
                let sparse_json = serde_json::to_value(&embed_out.sparse)
                    .context("serializing sparse embedding")?;
                sqlx::query("UPDATE memories SET sparse_embedding = $1 WHERE id = $2")
                    .bind(&sparse_json)
                    .bind(id)
                    .execute(&pool)
                    .await
                    .context("updating sparse_embedding")?;
                Ok::<_, anyhow::Error>(())
            });
            if join_set.len() >= concurrency {
                join_set
                    .join_next()
                    .await
                    .unwrap()?
                    .context("backfill task")?;
            }
        }
        while let Some(result) = join_set.join_next().await {
            result?.context("backfill task")?;
        }

        total += batch_len as u64;
        tracing::info!(batch = batch_len, total, "backfill batch complete");

        if (batch_len as i64) < batch_size {
            break;
        }
    }

    println!("Backfill complete: {total} rows updated");
    Ok(())
}

async fn run_reflect(profile: String, model: String, use_api: bool) -> Result<()> {
    let cfg = Config::from_env().context("loading configuration from environment")?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("info"))
        .with_writer(std::io::stderr)
        .init();

    let pool = db::connect(&cfg).await.context("connecting to database")?;
    db::run_migrations(&pool)
        .await
        .context("running migrations")?;

    let embedder = Embedder::load(
        &cfg.model_file(),
        &cfg.tokenizer_file(),
        cfg.embedder_pool_size,
        cfg.sparse_threshold,
    )
    .context("loading embedding model")?;

    if use_api {
        #[cfg(feature = "api")]
        {
            let llm = chitta::llm::ClaudeApiLlm::from_env(model)?;
            chitta::reflect::reflect_pipeline(&pool, &embedder, &llm, &profile).await?;
        }
        #[cfg(not(feature = "api"))]
        {
            anyhow::bail!(
                "--api-key requires the `api` feature: rebuild with `cargo build --features api`"
            );
        }
    } else {
        let llm = ClaudeCliLlm::new(model);
        chitta::reflect::reflect_pipeline(&pool, &embedder, &llm, &profile).await?;
    };

    Ok(())
}

/// Streamable HTTP transport with bearer-token auth.
async fn serve_http(
    cli: Cli,
    cfg: Config,
    pool: sqlx::PgPool,
    embedder: Arc<Embedder>,
    query_log_enabled: bool,
) -> Result<()> {
    use axum::routing::any_service;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    use tokio_util::sync::CancellationToken;
    use tower_http::validate_request::ValidateRequestHeaderLayer;

    let token_path = cli.auth_token_file.ok_or_else(|| {
        anyhow::anyhow!("--auth-token-file is required when running in --http mode")
    })?;
    let bearer_token = std::fs::read_to_string(&token_path)
        .with_context(|| format!("reading auth token from {}", token_path.display()))?
        .trim()
        .to_string();
    anyhow::ensure!(
        !bearer_token.is_empty(),
        "auth token file is empty after trimming whitespace"
    );
    anyhow::ensure!(
        bearer_token.chars().all(|c| !c.is_control()),
        "auth token contains control characters — regenerate the token file"
    );

    let cancel = CancellationToken::new();

    let http_addr = cli.http_addr.unwrap_or_else(|| cfg.http_addr.clone());
    let http_port = cli.http_port.unwrap_or(cfg.http_port);

    let mut allowed_hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if http_addr != "127.0.0.1" && http_addr != "localhost" && http_addr != "::1" {
        allowed_hosts.push(http_addr.clone());
    }

    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancel.clone())
        .with_allowed_hosts(allowed_hosts);

    let session_manager = Arc::new(LocalSessionManager::default());

    let pool_clone = pool.clone();
    let embedder_clone = Arc::clone(&embedder);
    let ql = query_log_enabled;
    let search_cfg = cfg.search.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(ChittaServer::new(
                pool_clone.clone(),
                Arc::clone(&embedder_clone),
                ql,
                search_cfg.clone(),
            ))
        },
        session_manager,
        config,
    );

    // rmcp requires Accept to contain both application/json and text/event-stream,
    // but some clients (e.g. Claude Code) omit it. Normalize before rmcp sees it.
    let normalize_accept = axum::middleware::from_fn(
        |mut req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
            use axum::http::header::ACCEPT;
            let dominated = req
                .headers()
                .get(ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_none_or(|v| {
                    !v.contains("application/json") || !v.contains("text/event-stream")
                });
            if dominated {
                req.headers_mut().insert(
                    ACCEPT,
                    "application/json, text/event-stream".parse().unwrap(),
                );
            }
            next.run(req).await
        },
    );

    let (ingest_tx, ingest_rx) = tokio::sync::mpsc::channel::<ingest::IngestItem>(256);
    let ingest_state = ingest::IngestState { tx: ingest_tx };

    tokio::spawn(ingest::run_extraction_worker(
        ingest_rx,
        pool.clone(),
        Arc::clone(&embedder),
    ));

    let authed = axum::Router::new()
        .route("/mcp", any_service(mcp_service))
        .route(
            "/ingest",
            axum::routing::post(ingest::ingest_handler).with_state(ingest_state),
        )
        .layer(normalize_accept)
        .layer(ValidateRequestHeaderLayer::bearer(&bearer_token));

    let health_pool = pool.clone();
    #[allow(deprecated)]
    let app = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(move || async move {
                let ok = sqlx::query_scalar::<_, i32>("SELECT 1")
                    .fetch_one(&health_pool)
                    .await
                    .is_ok();
                let status = if ok { "ok" } else { "degraded" };
                axum::Json(serde_json::json!({ "status": status }))
            }),
        )
        .merge(authed);

    let addr = format!("{http_addr}:{http_port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e} — is the port in use?"))?;
    tracing::info!(%addr, "chitta HTTP server listening");

    let cancel_for_shutdown = cancel.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received, draining connections");
            cancel_for_shutdown.cancel();
        })
        .await
        .context("HTTP server exited with error")?;

    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // Non-Unix fallback: Ctrl-C only (no SIGTERM equivalent). v0.0.1 targets
    // Linux/macOS; this keeps the code compiling on Windows for contributors
    // running `cargo check` locally.
    let _ = tokio::signal::ctrl_c().await;
}
