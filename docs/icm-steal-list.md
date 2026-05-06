# ICM steal list

What to adopt from [ICM](https://github.com/rtk-ai/icm) (Infinite Context Memory)
into chitta. Based on a side-by-side comparison (2026-05-06).

---

## Comparison summary

| Dimension | ICM | Chitta |
|---|---|---|
| **Storage** | SQLite + FTS5 + sqlite-vec (single file) | Postgres + pgvector + FTS (server) |
| **Embedder** | E5 family (ONNX, configurable) | BGE-M3 (ONNX, dense + sparse) |
| **Retrieval** | Hybrid: BM25 (30%) + cosine (70%), fixed weights | RRF fusion: dense + FTS + sparse, configurable legs |
| **Data model** | Memories (episodic) + Memoirs (knowledge graphs) + Feedback + Transcripts | Memories only (flat, typed) |
| **Write contract** | Auto-dedup (>85% similarity → update) | Idempotency keys, verbatim-is-sacred |
| **Temporality** | Single timestamp + weight decay | Bi-temporal (event_time + record_time) |
| **Lifecycle** | Decay by importance, consolidation, pruning | Append-only, no decay |
| **Isolation** | Topics (flat namespace) | Profiles (multi-tenant) |
| **Tools** | 31 MCP tools | 7 MCP tools (by principle) |
| **Hooks** | 5 hooks (start, pre, post, compact, prompt) — deep agent integration | None — pure MCP server |
| **Extraction** | Rule-based auto-extraction from tool output (zero LLM cost, 2000 lines) | None — agent stores explicitly |
| **Principles** | Implicit, product-driven | 11 explicit, documented, PR-enforced |
| **Audit** | Stats, health checks | Query log with full search replay |

---

## Steal list (priority order)

### 1. Auto-extraction via hooks

ICM's `extract.rs` is a 2000-line rule-based engine that pulls facts from
tool output without any LLM call. It classifies decisions, preferences,
errors, constraints, learnings — all with keyword + semantic anchor scoring.
Chitta currently relies entirely on the agent to decide what to store. The
agent forgets to store, stores poorly, or stores too late. Hook-driven
extraction would make chitta's memory accumulate passively.

Three extraction layers in ICM (all zero LLM cost):

| Layer | Hook | What it does |
|-------|------|-------------|
| 0 | `PostToolUse` | Rule-based keyword extraction from tool output |
| 1 | `PreCompact` | Extract from transcript before context compression |
| 2 | `UserPromptSubmit` | Inject recalled memories on each user prompt |

Key implementation details from ICM's extractor:
- Sentence splitting that handles URLs, file paths, version numbers, honorifics
- Narration filtering (strips "Let me check...", "I'll now read..." LLM filler)
- Keyword scoring with importance classification (decisions→high, preferences→critical)
- Semantic anchor scoring (embedder-based, multilingual) as an upgrade path
- Entity detection (person/project names via heuristic patterns)
- Jaccard dedup to avoid storing paraphrases
- Importance capping for untrusted content (hook output capped at medium)

### 2. Wake-up tool

`icm_wake_up` builds a project-scoped context injection of critical/high
memories at session start. Currently with chitta, the agent has to know to
search. A dedicated "give me what matters for project X" tool that returns a
curated, token-budgeted preamble removes that friction.

### 3. Temporal decay / importance weighting

Chitta treats all memories equally forever. ICM's decay model:

| Importance | Decay rate | Auto-prune? |
|-----------|-----------|-------------|
| `critical` | none | never |
| `high` | 0.5x rate | never |
| `medium` | 1.0x rate | yes, when weight < threshold |
| `low` | 2.0x rate | yes, when weight < threshold |

Access-aware: `decay / (1 + access_count * 0.1)`. Frequently recalled
memories resist decay. Applied on recall if >24h since last decay.

Without something like this, chitta will drown in stale observations over time.

### 4. Consolidation

When a topic exceeds N entries, ICM warns the caller and offers to merge them
into a summary. Chitta has no equivalent — profiles just grow unbounded. Some
form of "you have 200 observations tagged `project:foo`, want a summary?"
would help.

### 5. Feedback / correction loop

`icm_feedback_record` captures "agent predicted X, correct answer was Y".
Creates a searchable corpus of mistakes. Before making predictions, the agent
can check past corrections. Chitta has no learning-from-errors mechanism.

### 6. Hook architecture (selective)

The pre/post/compact hooks make extraction zero-effort. Chitta doesn't need
the permission-bypass hook (pre), but two patterns are useful:

- **Compact hook**: extract before context compression — saves knowledge that
  would otherwise be lost when the conversation is compacted.
- **Prompt hook**: inject recalled context per turn — the agent starts every
  turn with relevant memories already loaded.

---

## What NOT to steal

- **31 tools** — ICM's tool sprawl is exactly what chitta's Principle 5 guards
  against. Keep the small surface.
- **Memoirs / knowledge graphs** — Interesting but unproven. No evidence agents
  use them effectively. If chitta needs structured knowledge, it should come
  from a benchmark win, not feature-parity envy.
- **Auto-dedup by similarity** — ICM dedupes at >85% cosine on write. Chitta's
  idempotency-key approach is more principled — dedup is the client's decision,
  not a server-side heuristic that can silently merge distinct memories.
- **SQLite-everything** — Single-file is great for distribution, but chitta
  chose Postgres for good reasons (concurrency, pgvector HNSW, real
  transactions). Don't regress.
- **Transcripts** — Chitta already has `.sessions/` JSONL files for session
  replay. Duplicating transcript storage inside the memory DB is unnecessary.

---

## Core insight

The agent is unreliable as a memory curator. The more you can automate the
write path (extraction, classification, importance scoring) without LLM calls,
the better the memory actually works. Chitta's retrieval is more sophisticated,
but ICM's write-side automation is where the real UX advantage lives.
