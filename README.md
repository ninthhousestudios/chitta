# chitta

Agent-native persistent memory server, speaking MCP over stdio or
Streamable HTTP. Postgres + pgvector backend, BGE-M3 ONNX embedder,
seven tools, verbatim storage, bi-temporal rows, typed memories,
idempotent writes, actionable errors.

See [`docs/principles.md`](docs/principles.md) for the contract this server
upholds.

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

## Tools

| Tool | Purpose |
|---|---|
| `store_memory` | Persist verbatim content with optional `event_time`, `tags`, `memory_type`, `metadata`. Idempotent on `(profile, idempotency_key)`. |
| `get_memory` | Fetch one memory by profile + UUID. |
| `search_memories` | Semantic search with tag/type filters, similarity floor, and token-budget truncation. |
| `update_memory` | Update content, tags, type, or metadata. Re-embeds on content change. |
| `delete_memory` | Hard-delete by profile + id. |
| `list_recent_memories` | List by `record_time` DESC with tag/type filters. |
| `health_check` | Verify DB connectivity and embedder responsiveness. |

Memory types: `memory`, `observation`, `decision`, `session_summary`, `mental_model`.

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

## CLI subcommands

| Command | Purpose |
|---|---|
| `serve` | Run as MCP server (default). |
| `replay` | Re-run logged queries for retrieval regression detection. |
| `backfill` | Backfill sparse embeddings for rows that have none. |
