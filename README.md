# chitta

[![License: MPL 2.0](https://img.shields.io/badge/License-MPL_2.0-brightgreen.svg)](https://opensource.org/licenses/MPL-2.0)

Chitta is the working model of the human in an AI-assisted workflow. It
stores and retrieves what the person values, how they think, what patterns
they follow, and what preferences they hold — so that every agent session
starts grounded in who it's working with, not just what it's working on.

The Sanskrit *citta* (चित्त) means the field of impressions that conditions
future thought and behavior. That's the role this subsystem plays: it
accumulates observations across sessions, synthesizes them into stable
traits and preferences, and surfaces them automatically so agents act
consistently with the human they serve.

Part of [manas](https://github.com/ninthhousestudios/manas), a modular agent
infrastructure built in Rust.

## What chitta is (and isn't)

Chitta is **not** a general-purpose memory store. Other manas subsystems
handle project tasks (yojana), code intelligence (sutra), file indexing
(smriti), and document retrieval (kosha). Chitta holds only content that
models the person — observations about their corrections, decisions that
reveal their values, patterns in how they work.

A memory belongs in chitta if it would help an agent act more like a
trusted colleague who knows the human well. If it's about a codebase or a
task, it belongs somewhere else.

## Design

Memories live in three layers:

- **Raw / episodic** — observations, episodes, and decisions captured
  during sessions. Append-only, immutable.
- **Consolidated / semantic** — traits, values, patterns, preferences,
  and mental models synthesized from raw material. Confidence-weighted,
  supersedeable (old versions are kept, not deleted).
- **Profile / always-on** — the top ~30 consolidated entries by effective
  score, loaded at session start without any query. This is what makes the
  working model actually work: agents get the most important facts about
  the human before they do anything else.

Retrieval has three tiers: the always-on profile (no query needed),
context-faceted SQL filtering (by domain, skill, project, situation), and
hybrid dense+sparse semantic search for everything else.

See [`docs/principles.md`](docs/principles.md) for the invariants this
server upholds — verbatim storage, bi-temporal rows, write-fast/enrich-lazy,
agent-native wire contract.

## Prerequisites

- **Rust** stable (edition 2024 — 1.85+).
- **Postgres 16+** with the [`pgvector`](https://github.com/pgvector/pgvector)
  extension installed.
- **BGE-M3 ONNX model** — `bge_m3_model.onnx` (plus `.onnx_data` sidecar)
  and `tokenizer.json`. Default location: `~/.chitta/models/bge-m3-onnx/`.
  The upstream [BAAI/bge-m3](https://huggingface.co/BAAI/bge-m3) ONNX export
  works, as does a custom export with dense/sparse heads.
- **ONNX Runtime shared library.** Either install `onnxruntime` via your
  package manager or reuse the copy shipped with Python's `onnxruntime`
  wheel; point `ORT_DYLIB_PATH` at it if it's not on the default loader path.

## Install

```bash
createdb chitta
psql chitta -c 'create extension if not exists vector'
cargo install --path .
```

This places the `chitta` binary in `~/.cargo/bin/`.

## Configuration

All configuration is via environment variables. Place a `.env` file at
`~/.chitta/.env` — it is loaded automatically at startup. A `.env` in the
working directory is also loaded as a fallback.

| Variable | Default | Notes |
|---|---|---|
| `CHITTA_HOME` | `~/.chitta` | Data directory root. |
| `DATABASE_URL` | `postgresql://localhost/chitta` | libpq-compatible Postgres URL. |
| `CHITTA_MODEL_PATH` | `~/.chitta/models/bge-m3-onnx` | Directory with the ONNX model + tokenizer. |
| `CHITTA_LOG_LEVEL` | `info` | `tracing_subscriber` env filter syntax. |
| `CHITTA_HTTP_ADDR` | `127.0.0.1` | HTTP listen address (with `--http`). |
| `CHITTA_HTTP_PORT` | `3100` | HTTP listen port (with `--http`). |
| `ORT_DYLIB_PATH` | *(loader default)* | Path to `libonnxruntime.so`. |

See [`.env.example`](.env.example) for the full list including pool tuning
and retrieval scoring knobs.

## Running

### Stdio (default)

The binary is a stdio-transport MCP server. It reads JSON-RPC from stdin
and writes responses to stdout; logs go to stderr.

```json
{
  "mcpServers": {
    "chitta": {
      "command": "/home/you/.cargo/bin/chitta"
    }
  }
}
```

### Streamable HTTP

```bash
chitta --http --auth-token-file ~/.chitta/bearer-token.txt
```

Serves on `http://127.0.0.1:3100/mcp` with bearer-token auth. Client config:

```json
{
  "mcpServers": {
    "chitta": {
      "type": "http",
      "url": "http://127.0.0.1:3100/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

### systemd (user service)

```ini
[Unit]
Description=chitta MCP memory server (HTTP)
After=postgresql.service

[Service]
EnvironmentFile=%h/.chitta/.env
ExecStartPre=/bin/sh -c 'until pg_isready -q; do sleep 1; done'
ExecStart=%h/.cargo/bin/chitta --http --auth-token-file %h/.chitta/bearer-token.txt
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
```

## Status

Chitta is running in production and being actively redesigned. The current
server (v0.3) works — it stores memories, embeds them, and retrieves them
via hybrid search. But it was built as a generic memory store and is being
narrowed to its actual job: modelling the human.

What's landed:
- Core store/search/update tools with bi-temporal rows
- Dense (BGE-M3 1024-dim) + sparse vector retrieval with RRF
- `get_profile` for always-on session grounding
- `supersede_memory` for first-class trait evolution
- Memory types: `observation`, `episode`, `decision`, `trait`, `value`,
  `pattern`, `preference`, `mental_model`
- `applies_to` facets (domains, skills, projects, situations) for
  context-scoped retrieval
- Query logging for retrieval regression detection

What's in progress:
- Schema migration to enforce the three-layer taxonomy
- Gold-set evaluation (~50 hand-authored retrieval test cases)
- `/reflect` rewrite for automated synthesis (raw -> consolidated)
- `/agree` and `/disagree` feedback loops for reinforcement

See [`docs/working-model-pivot.md`](docs/working-model-pivot.md) for the
full design direction.

## Tools

| Tool | Purpose |
|---|---|
| `store_memory` | Persist verbatim content. Idempotent on `(profile, idempotency_key)`. |
| `get_memory` | Fetch one memory by profile + id (prefix match). |
| `search_memories` | Hybrid dense+sparse semantic search with facet filters. |
| `get_profile` | Load the always-on working model (~30 top entries, no query). |
| `supersede_memory` | Replace a consolidated memory, preserving the old version. |
| `update_memory` | Update content, tags, type, or metadata. Re-embeds on content change. |
| `delete_memory` | Hard-delete (for genuine mistakes; prefer supersession). |
| `list_recent_memories` | List by recency with tag/type filters. |
| `reflect_status` | Check synthesis pipeline health. |
| `health_check` | Verify DB connectivity and embedder responsiveness. |

## Testing

### Unit tests

```bash
cargo test --lib
```

### Integration tests

Require a live Postgres with pgvector and the ONNX model on disk:

```bash
createdb chitta_test
psql chitta_test -c 'create extension if not exists vector'
export TEST_DATABASE_URL=postgres://localhost/chitta_test
cargo test --test integration
```

Tests without `TEST_DATABASE_URL` set (or with the model missing) print a
`SKIPPED:` line and pass.

### Lint

```bash
cargo clippy --all-targets -- -D warnings
```

## Architecture

Chitta is a Rust binary (~6k LOC) that speaks
[MCP](https://modelcontextprotocol.io) over stdio or Streamable HTTP.

- **Storage:** Postgres 16+ with pgvector. Bi-temporal rows, typed
  memories, idempotent writes.
- **Embedding:** BGE-M3 via ONNX Runtime, in-process. Produces both dense
  (1024-dim) and sparse vectors per memory. No external API calls on the
  write path.
- **Retrieval:** Reciprocal Rank Fusion over dense and sparse results,
  with optional facet filtering. Token-budget-aware truncation.
- **Transport:** Stdio (for direct MCP client use) or HTTP with
  bearer-token auth (for systemd service deployment).

## CLI subcommands

| Command | Purpose |
|---|---|
| `serve` | Run as MCP server (default). |
| `replay` | Re-run logged queries for retrieval regression detection. |
| `backfill` | Backfill sparse embeddings for rows that have none. |
