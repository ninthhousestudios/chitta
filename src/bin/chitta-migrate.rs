//! chitta-migrate — throwaway export/seed CLI for chitta DB migration.
//!
//! Export: reads memories from a source Postgres DB, writes JSONL.
//! Seed:   reads JSONL, validates through chitta's write-time validators,
//!         embeds, and inserts into the target DB. Each row is tagged
//!         `seed:2026-05`.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use chitta::config::{Config, chitta_home};
use chitta::db::{self, MemoryRow};
use chitta::embedding::Embedder;
use chitta::error::ChittaError;
use chitta::facets::Facets;
use chitta::tools::validate;
use chitta::validators;
use chitta::validators::DerivationInput;

const SEED_TAG: &str = "seed:2026-05";
const TOOL: &str = "chitta-migrate";

#[derive(Parser)]
#[command(
    name = "chitta-migrate",
    version,
    about = "Export/seed migration tool for chitta DB"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Export memories from a source DB as JSONL (no embeddings).
    Export {
        /// PostgreSQL connection URL for the source database.
        #[arg(long)]
        source: String,
        /// Output JSONL file path.
        #[arg(long)]
        out: String,
    },
    /// Seed memories from a JSONL file into the target DB (DATABASE_URL).
    Seed {
        /// Input JSONL file path.
        #[arg(long)]
        from: String,
        /// Validate only; report rejections without writing to the database.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct MigrateRow {
    #[serde(default)]
    id: Option<Uuid>,
    profile: String,
    content: String,
    #[serde(default)]
    event_time: Option<DateTime<Utc>>,
    #[serde(default)]
    record_time: Option<DateTime<Utc>>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    external_refs: Option<serde_json::Value>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(flatten)]
    facets: Facets,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    derivations: Option<Vec<DerivationInput>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::from_path(chitta_home().join(".env"));
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    match cli.command {
        Command::Export { source, out } => run_export(&source, &out).await,
        Command::Seed { from, dry_run } => run_seed(&from, dry_run).await,
    }
}

// ── Export ───────────────────────────────────────────────────────────

async fn run_export(source_url: &str, out_path: &str) -> Result<()> {
    eprintln!("connecting to source DB…");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(source_url)
        .await
        .context("connecting to source database")?;

    let query = build_export_query(&pool).await?;
    eprintln!("query: {query}");

    let rows: Vec<(String,)> = sqlx::query_as(&query)
        .fetch_all(&pool)
        .await
        .context("executing export query")?;

    let file =
        std::fs::File::create(out_path).context(format!("creating output file: {out_path}"))?;
    let mut writer = BufWriter::new(file);

    for (row_json,) in &rows {
        writeln!(writer, "{row_json}")?;
    }
    writer.flush()?;

    eprintln!("exported {} rows to {out_path}", rows.len());
    Ok(())
}

async fn build_export_query(pool: &PgPool) -> Result<String> {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_name = 'memories' AND table_schema = 'public' \
         ORDER BY ordinal_position",
    )
    .fetch_all(pool)
    .await
    .context("querying information_schema")?;

    let has = |name: &str| columns.iter().any(|c| c == name);

    let mut parts: Vec<String> = vec![
        "'id', id".into(),
        "'profile', profile".into(),
        "'content', content".into(),
    ];

    if has("event_time") {
        parts.push("'event_time', event_time".into());
        parts.push("'record_time', record_time".into());
    } else if has("created_at") {
        parts.push("'event_time', created_at".into());
        parts.push("'record_time', created_at".into());
    }

    if has("idempotency_key") {
        parts.push("'idempotency_key', idempotency_key".into());
    } else {
        parts.push("'idempotency_key', 'seed-' || id::text".into());
    }

    parts.push("'source', source".into());

    if has("memory_type") {
        parts.push("'memory_type', COALESCE(memory_type, 'observation')".into());
    } else {
        parts.push("'memory_type', 'observation'::text".into());
    }

    parts.push("'tags', COALESCE(tags, '{}')".into());

    for col in ["external_refs", "metadata"] {
        if has(col) {
            parts.push(format!("'{col}', {col}"));
        }
    }
    for col in [
        "applies_to_domains",
        "applies_to_skills",
        "applies_to_projects",
        "applies_to_situations",
    ] {
        if has(col) {
            parts.push(format!("'{col}', {col}"));
        }
    }
    if has("confidence") {
        parts.push("'confidence', confidence".into());
    }

    let json_expr = format!("json_build_object({})", parts.join(", "));
    let mut query = format!("SELECT ({json_expr})::text as row_json FROM memories");

    if has("invalidated_at") {
        query.push_str(" WHERE invalidated_at IS NULL");
    }

    query.push_str(" ORDER BY ");
    if has("record_time") {
        query.push_str("record_time");
    } else if has("created_at") {
        query.push_str("created_at");
    } else {
        query.push_str("id");
    }

    Ok(query)
}

// ── Seed ────────────────────────────────────────────────────────────

async fn run_seed(from_path: &str, dry_run: bool) -> Result<()> {
    let rows = read_jsonl(from_path)?;
    eprintln!("read {} rows from {from_path}", rows.len());

    let mut ok_rows: Vec<(usize, MigrateRow)> = Vec::new();
    let mut reject_count = 0usize;

    for (i, row) in rows.into_iter().enumerate() {
        match validate_row(&row) {
            Ok(()) => ok_rows.push((i, row)),
            Err(msg) => {
                eprintln!("REJECT row {}: {msg}", i + 1);
                reject_count += 1;
            }
        }
    }

    eprintln!(
        "\nvalidation: {} ok, {reject_count} rejected",
        ok_rows.len()
    );

    if dry_run {
        eprintln!("dry-run complete. No rows written.");
        return Ok(());
    }

    if ok_rows.is_empty() {
        eprintln!("no valid rows to seed.");
        return Ok(());
    }

    let cfg = Config::from_env().context("loading config (need DATABASE_URL + model path)")?;
    let pool = db::connect(&cfg).await.context("connecting to target DB")?;
    db::run_migrations(&pool)
        .await
        .context("running migrations on target")?;

    eprintln!("loading embedder…");
    let embedder = Embedder::load(
        &cfg.model_file(),
        &cfg.tokenizer_file(),
        cfg.embedder_pool_size,
        cfg.sparse_threshold,
    )
    .context("loading embedding model")?;
    let embedder = Arc::new(embedder);

    let total = ok_rows.len();
    let mut inserted = 0usize;
    let mut replayed = 0usize;
    let mut seed_errors = 0usize;

    for (idx, (i, row)) in ok_rows.into_iter().enumerate() {
        match seed_one_row(&pool, &embedder, row).await {
            Ok(was_replay) => {
                if was_replay {
                    replayed += 1;
                } else {
                    inserted += 1;
                }
            }
            Err(e) => {
                eprintln!("ERROR row {}: {e:#}", i + 1);
                seed_errors += 1;
            }
        }
        if (idx + 1) % 50 == 0 {
            eprintln!("  progress: {}/{total}", idx + 1);
        }
    }

    eprintln!(
        "\nseed complete: {inserted} inserted, {replayed} idempotent replays, {seed_errors} errors"
    );
    Ok(())
}

fn read_jsonl(path: &str) -> Result<Vec<MigrateRow>> {
    let file = std::fs::File::open(path).context(format!("opening {path}"))?;
    let reader = BufReader::new(file);
    let mut rows = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.context(format!("reading line {}", i + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let row: MigrateRow =
            serde_json::from_str(trimmed).context(format!("parsing JSON at line {}", i + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

fn validate_row(row: &MigrateRow) -> std::result::Result<(), String> {
    let memory_type = row.memory_type.as_deref().unwrap_or("observation");

    validate::profile(TOOL, &row.profile).map_err(format_err)?;
    validate::content_non_empty(TOOL, &row.content).map_err(format_err)?;
    validate::content_byte_length(TOOL, &row.content).map_err(format_err)?;
    validators::memory_type(TOOL, memory_type).map_err(format_err)?;

    if let Some(ref tags) = row.tags {
        validate::tags(TOOL, tags).map_err(format_err)?;
    }
    if let Some(ref refs) = row.external_refs {
        validators::external_refs(TOOL, refs).map_err(format_err)?;
    }

    validators::validate_decision_metadata(TOOL, memory_type, &row.metadata).map_err(format_err)?;
    validators::episode_derivations(TOOL, memory_type, &row.derivations).map_err(format_err)?;

    Ok(())
}

fn format_err(e: ChittaError) -> String {
    let data = e.data();
    format!(
        "[{}] {} — next: {}",
        data.argument.as_deref().unwrap_or("?"),
        data.constraint,
        data.next_action,
    )
}

async fn seed_one_row(pool: &PgPool, embedder: &Arc<Embedder>, row: MigrateRow) -> Result<bool> {
    let now = Utc::now();
    let memory_type = row.memory_type.unwrap_or_else(|| "observation".to_string());
    let id = row.id.unwrap_or_else(Uuid::now_v7);
    let idempotency_key = row.idempotency_key.unwrap_or_else(|| format!("seed-{id}"));
    let event_time = row.event_time.unwrap_or(now);

    let mut tags = row.tags.unwrap_or_default();
    if !tags.iter().any(|t| t == SEED_TAG) {
        tags.push(SEED_TAG.to_string());
    }

    let embed_out = embedder
        .embed_full(&row.content, TOOL)
        .await
        .context("embedding content")?;
    let sparse_embedding = if embed_out.sparse.is_empty() {
        None
    } else {
        Some(serde_json::to_value(&embed_out.sparse)?)
    };

    let mem = MemoryRow {
        id,
        profile: row.profile,
        content: row.content,
        embedding: Some(Vector::from(embed_out.dense)),
        sparse_embedding,
        event_time,
        record_time: row.record_time.unwrap_or(now),
        idempotency_key,
        source: row.source,
        memory_type,
        tags,
        external_refs: row.external_refs,
        metadata: row.metadata,
        facets: row.facets,
        superseded_by: None,
        confidence: row.confidence,
        reinforcement_count: 0,
        last_reinforced_at: None,
        invalidated_at: None,
    };

    let parsed_derivations: Vec<(Uuid, String)> = row
        .derivations
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            let uuid = Uuid::parse_str(&d.source_id).expect("validated");
            (uuid, d.derivation_type)
        })
        .collect();

    let (_, replayed) = if parsed_derivations.is_empty() {
        db::insert_or_fetch_memory(pool, &mem).await?
    } else {
        db::insert_memory_with_derivations(pool, &mem, &parsed_derivations).await?
    };

    Ok(replayed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_row(overrides: serde_json::Value) -> MigrateRow {
        let mut base = json!({
            "profile": "josh",
            "content": "test content",
            "idempotency_key": "test-key",
            "memory_type": "observation"
        });
        if let (Some(base_obj), Some(over_obj)) = (base.as_object_mut(), overrides.as_object()) {
            for (k, v) in over_obj {
                base_obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn valid_observation_passes() {
        assert!(validate_row(&make_row(json!({}))).is_ok());
    }

    #[test]
    fn valid_decision_passes() {
        let row = make_row(json!({
            "memory_type": "decision",
            "metadata": {
                "rationale": "chose X for speed",
                "rejected_alternatives": ["Y was slower"]
            }
        }));
        assert!(validate_row(&row).is_ok());
    }

    #[test]
    fn decision_without_rationale_rejected() {
        let row = make_row(json!({
            "memory_type": "decision",
            "metadata": {}
        }));
        let err = validate_row(&row).unwrap_err();
        assert!(err.contains("rationale"), "error mentions rationale: {err}");
    }

    #[test]
    fn episode_without_derivations_rejected() {
        let row = make_row(json!({ "memory_type": "episode" }));
        let err = validate_row(&row).unwrap_err();
        assert!(
            err.contains("derivation"),
            "error mentions derivations: {err}"
        );
    }

    #[test]
    fn empty_content_rejected() {
        let row = make_row(json!({ "content": "" }));
        assert!(validate_row(&row).is_err());
    }

    #[test]
    fn fixture_file_validates() {
        let fixture_path = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/migrate/fixture.jsonl");
        let rows = read_jsonl(fixture_path).expect("fixture should parse");
        assert!(rows.len() >= 5, "fixture needs at least 5 rows");

        let mut pass = 0;
        let mut fail = 0;
        for row in &rows {
            match validate_row(row) {
                Ok(()) => pass += 1,
                Err(_) => fail += 1,
            }
        }
        assert!(pass >= 4, "at least 4 rows should pass, got {pass}");
        assert!(
            fail >= 1,
            "at least 1 row should fail (deliberate), got {fail}"
        );
    }
}
