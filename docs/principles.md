# chitta — foundational principles

Reference doc. These principles override convenience. If a proposed change violates one, the change loses unless the principle is explicitly revised here first.

Chitta is a cognitive confluence — where human knowledge and agent capability meet. These principles govern how it's built.

---

## 1. Verbatim is sacred

The source text of a memory, as stored, is immutable. Everything else (embeddings, extracted entities, PageRank, edges, FSRS fields, summaries) is *derived* and must be re-derivable from the source + deterministic pipelines.

**Rules out:** mutating `content` in place. "Cleanup" passes that rewrite stored text. Storing only a summary and discarding the source.

## 2. Bi-temporal by default

Every memory row is born with two timestamps:
- `event_time` — when the thing happened in the world.
- `record_time` — when chitta learned about it.

Corrections are new rows that supersede, not edits. The audit trail is append-only.

**Rules out:** single-timestamp schemas. In-place UPDATE of content or time fields. Deletes that don't leave a tombstone.

## 3. Write fast, enrich lazily

The write path does the minimum: validate → embed → insert → return. Everything else (entity extraction, edge creation, PageRank updates, FSRS bookkeeping) runs in a background worker reading from a queue. Background work never blocks a response. This is an invariant, not a goal.

**Rules out:** synchronous LLM calls on the write path. Extraction pipelines in `store_memory`. Any "just a quick sync step" that grows.

## 4. Agent-native wire contract

Every retrieval tool returns the envelope `{ results, truncated, total_available, budget_spent_tokens }`. Every response respects a caller-supplied `max_tokens` budget and reports actual spend. Schema-tested; deviations break the build.

**Rules out:** ad-hoc response shapes per tool. Token-unbounded results. Silent truncation.

## 5. Small core, grow by evidence

v0.0.1 is three tools: `store_memory`, `get_memory`, `search_memories`. Every additional tool must justify itself against either a named benchmark win or a recurring real-use case documented in the release notes. The Python tree's 38-tool sprawl is the anti-pattern being escaped.

**Rules out:** speculative tools. Tools added "because the upstream had them." Tools added without a removal-criterion.

## 6. Idempotent writes

Every write accepts a client-supplied `idempotency_key`. Re-sending the same key returns the prior result without side effects. The server never silently de-duplicates without a key — that's the client's job to opt into.

**Rules out:** retry semantics that depend on server-side heuristics. Content-hash dedup as the *only* dedup path (fine as a layered signal, not as the contract).

## 7. Profiles are the only isolation primitive

A memory belongs to exactly one profile. Profile is a required argument on every tool (no implicit "current profile" server state). This is what lets the same binary go from single-user to multi-tenant later without a rewrite.

**Rules out:** sticky session state on the server. Implicit profile resolution from connection identity. Any secondary namespace concept (workspaces, tenants, rooms) before v1.0.

## 8. Errors are instructions

Every error response names: (a) the tool and offending argument, (b) the constraint that was violated, (c) the next action the caller can take. Error-message actionability is a tracked Track 3 metric, not a nice-to-have.

**Rules out:** `Error: invalid input`. Stack traces in protocol responses. Errors that don't tell the agent what to try next.

## 9. No write-time extraction until it wins a benchmark

No entity extraction, no temporal parsing, no language-pack lookups on the write path in v0.0.1. These earn their place only when a Track 1 or Track 2 benchmark shows them improving retrieval. The 16-language YAML pack in the Python tree was overbuild — don't port it on faith.

**Rules out:** porting `extraction.py` on day one. Adding language packs before the retrieval primitives are measured. NLP helpers that aren't called by any tool.

## 10. Every dependency has a written reason

Each entry in `Cargo.toml` has a one-line comment stating what it's for and what was considered instead. If no reason can be written, the dependency isn't taken. This is the direct answer to the "I don't trust this code" complaint — chitta will only contain code whose presence is justified.

**Rules out:** silent dependency growth. "Might as well add X." Transitive dependency bloat via convenience crates.

## 11. The human owns the data

Chitta is self-hosted because the knowledge inside it belongs to the human, not to a service provider. Every memory is exportable. Every derived structure (graphs, summaries, learning signals) is re-derivable from the source corpus. The human can inspect, query, and understand their own data without going through an agent. Human-readable formats, direct DB access, and knowledge views are not afterthoughts — they're part of the contract.

**Rules out:** cloud-only storage. Opaque derived state that can't be explained. Features that only work through an agent interface. Lock-in by complexity.

---

## How this doc is used

- Every PR description references the principles it touches (either upholding or revising).
- Revising a principle requires its own PR that updates this doc first, then lands the behavior change in a follow-up.
- Principles are numbered for stable reference; do not renumber. If one is retired, mark it `withdrawn` in place.

## Companion docs

- `docs/research/master-plan.md` — strategic roadmap (authoritative for the *what*; this doc governs the *how*).
- `docs/pre-v0.0.3-roadmap.md` — current experiment plan and version scope.
