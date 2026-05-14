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
| `ANTHROPIC_API_KEY` | *(unset)* | Required for `chitta reflect` (API backend, the default). |
| `CHITTA_REFLECT_MIN_CLUSTER_SIZE` | `5` | Minimum source rows per cluster before emission. |
| `CHITTA_REFLECT_MIN_DISTINCT_DAYS` | `2` | Source rows must span at least this many distinct days. |
| `CHITTA_REFLECT_MAX_SOURCE_AGE_DAYS` | `90` | At least one source row must be within this many days. |

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
- `record_feedback` plus Claude `/agree` and `/disagree` commands for
  reinforcing or challenging the working model
- `reflect_status` for checking what raw evidence is waiting to be
  synthesized
- `chitta reflect` for automated raw -> consolidated synthesis
- Memory types: `observation`, `episode`, `decision`, `trait`, `value`,
  `pattern`, `preference`, `mental_model`
- `applies_to` facets (domains, skills, projects, situations) for
  context-scoped retrieval
- Query logging for retrieval regression detection

What's in progress:
- Schema migration to enforce the three-layer taxonomy
- Gold-set evaluation (~50 hand-authored retrieval test cases)

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
| `record_feedback` | Agree/disagree with a consolidated memory; used by `/agree` and `/disagree`. |
| `update_memory` | Update content, tags, type, or metadata. Re-embeds on content change. |
| `delete_memory` | Hard-delete (for genuine mistakes; prefer supersession). |
| `list_recent_memories` | List by recency with tag/type filters. |
| `reflect_status` | Check synthesis pipeline health. |
| `health_check` | Verify DB connectivity and embedder responsiveness. |

## Human workflow

Chitta is mostly agent-facing, but the working model needs human feedback.
The loop is:

1. Review the current working model in a Claude session.
2. Use `/agree` when a consolidated memory is still true.
3. Use `/disagree` when a consolidated memory is wrong, stale, or missing
   important nuance.
4. Run `chitta reflect` periodically to synthesize new raw evidence into
   consolidated memories.

### Review the working model

At the start of a session, ask Claude to load the profile:

```text
Use chitta get_profile for profile josh and show me the working model.
```

`get_profile` returns the active consolidated memories that currently matter
most. These are the memories that `/agree` and `/disagree` are meant to target:
`trait`, `value`, `pattern`, `preference`, and `mental_model` rows.

You can also ask Claude to search for a specific belief before giving feedback:

```text
Search chitta for memories about how I like code reviews.
```

Feedback requires a concrete memory UUID. Prefixes are fine if they resolve
unambiguously, but there is no "last memory" shorthand.

### Reinforce true memories with `/agree`

Claude command file: `~/.claude/commands/agree.md`.

Use `/agree` when a working-model entry rings true and should become more
durable:

```text
/agree 018f2c9a-...
/agree 018f2c9a-... 018f2cb1-...
```

The command calls `record_feedback` with `kind: "agree"` for each memory. That:

- raises confidence by `0.05`, capped at `1.0`
- increments `reinforcement_count`
- updates `last_reinforced_at`
- writes a raw feedback observation tagged `feedback` and `agree`

If you do not know the UUID, describe the memory and let Claude resolve it from
the current session context:

```text
/agree the one about preferring concise implementation notes
```

Claude should ask for a concrete ID if it cannot identify the target
confidently.

### Challenge wrong memories with `/disagree`

Claude command file: `~/.claude/commands/disagree.md`.

Use `/disagree` when a consolidated memory is wrong or stale:

```text
/disagree 018f2c9a-...
/disagree 018f2c9a-... 018f2cb1-...
```

The command calls `record_feedback` with `kind: "disagree"`. That lowers
confidence by `0.10`, floored at `0.0`, and writes a raw feedback observation
tagged `feedback` and `disagree`.

Add a correction after `--` when you know what should replace or refine the
memory:

```text
/disagree 018f2c9a-... -- I prefer direct implementation notes only after the risk is clear.
```

With a correction, `record_feedback` also writes a separate raw observation
tagged `correction` and `contradicts:<memory_id>`. The next `chitta reflect`
run uses that correction as contradicting evidence and can supersede the old
consolidated memory with a better one.

### Synthesize with `chitta reflect`

Run reflect after enough raw evidence has accumulated, or after recording
important corrections:

```bash
chitta reflect --profile josh
```

By default this uses the Anthropic API with prompt caching (reads
`ANTHROPIC_API_KEY` from the environment or `.env`):

```bash
chitta reflect --profile josh
```

Override the model if needed:

```bash
chitta reflect --profile josh --model claude-opus-4-6
```

To use the local Claude CLI subscription instead (slower — spawns one
process per row):

```bash
chitta reflect --profile josh --cli
```

Reflect reads raw `observation`, `episode`, and `decision` rows since the last
synthesis run for the profile. It asks the LLM to extract candidate claims,
cluster similar claims, check new claims against existing consolidated
memories, and emit synthesized rows tagged `reflect` and `synthesised`.

The default emission threshold is conservative: a cluster must have at least
five source rows, span at least two distinct record-time days, and use source
rows no older than 90 days. Tune these via environment variables to bootstrap
a fresh working model or tighten quality later:

```bash
# Lower thresholds for first run / small corpus:
CHITTA_REFLECT_MIN_CLUSTER_SIZE=2 CHITTA_REFLECT_MIN_DISTINCT_DAYS=1 chitta reflect --profile josh
```

New consolidated memories start with confidence based on cluster size, and
contradictory claims supersede the old active memory instead of deleting it.

The command prints a summary such as:

```text
reflect: 12 raw rows since 2026-05-10 14:20:00 UTC
synthesis: clusters_formed=3, clusters_emitted=2, supersessions=1
```

If there is nothing new to process, it prints:

```text
reflect: nothing to synthesize for profile 'josh'
```

### Seed the working model with `/onboard`

Skill file: [`onboard.md`](onboard.md).

Use `/onboard` to run a structured Q&A interview that seeds (or enriches) the
working model with intentional, high-signal observations. This produces cleaner
material than the indirect observations agents store during normal sessions.

The workflow:

1. Load the current profile and identify gaps.
2. Ask 3-4 questions per round across facets: values, workflow, background,
   collaboration style, preferences, aspirations.
3. Store raw observations after each round.
4. Consolidate answers into typed memories (`trait`, `value`, `pattern`,
   `preference`, `mental_model`).
5. Present a summary for the human to review and correct.

Run it when bootstrapping a fresh profile or when the existing profile feels
thin. The skill adapts questions based on what's already consolidated, so
re-running it is safe.

### Check pending evidence

From an MCP-enabled session, call `reflect_status` before a full synthesis run:

```text
Use chitta reflect_status for profile josh.
```

`reflect_status` counts raw rows since the last status check, reports the date
range and memory-type breakdown, and notes any disagree-flagged memory IDs. It
does not synthesize or write consolidated memories; `chitta reflect` does that.

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
| `reflect` | Run working-model synthesis for a profile. |

### `reflect`

```bash
chitta reflect --profile josh [--model claude-sonnet-4-6] [--cli]
```

By default, reflect uses the Anthropic API with prompt caching. Pass `--cli`
to use the local `claude` CLI instead (slower for batch workloads).
