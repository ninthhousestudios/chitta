# Release Review Synthesis: chitta upgrade arc

**Date:** 2026-05-11
**Scope:** Upgrade arc — facets refactor through reflect pipeline (`141ee8f..503472c`, 16 commits)
**Verdict:** ship with follow-ups
**Sources:**
- [Architecture pass](2026-05-11-upgrade-arc-architecture.md) — 6 deepening candidates
- [Code review pass](2026-05-11-upgrade-arc-review.md) — 7 findings, 8 slop items

## Convergent root causes

Both passes independently hit the same disease from different angles. These are the highest-leverage findings:

### 1. Synthesis pipeline drops source context at emission

**Architecture:** Candidate #3 — facets are `Facets::default()` on all emissions, breaking tier-1 retrieval.
**Review:** Finding #1 (high, correctness) — same diagnosis, same fix recommendation (`Facets::distinct_union` over source rows).
**Also related:** Review finding #4 (meta-row hardcoded to `mental_model` regardless of source type) and finding #6 (feedback evidence rows with NULL embeddings).

**Root cause:** The emission code paths in `synthesis.rs` create new rows in isolation — they don't carry forward the context (facets, type, embeddings) from the source material they were synthesized from. This is the single highest-impact finding: it silently degrades retrieval quality for all consolidated content, which is the layer `get_profile` and tier-1 search rely on.

### 2. synthesis.rs concentrates too much responsibility

**Architecture:** Candidate #1 — 1164 LOC, 6 distinct responsibilities, cognitive max 17.
**Review:** Design section — notes N+2 serial LLM calls with no internal boundaries; finding #5 (no timeout/budget) and finding #7 (full corpus sent to contradiction detection) are symptoms.

**Root cause:** Everything lives in one flat file with no internal modularity. Adding LLM timeouts, batching, or pre-filtering requires reasoning about all six concerns at once. Splitting into submodules (extraction, clustering, threshold, emission, contradiction) would make each of these follow-up fixes local.

### 3. Silent pipeline degradation

**Architecture:** Candidate #5 — `run_synthesis` returns `Ok` with zeros when all LLM calls fail.
**Review:** Finding #5 — no timeout means a hung subprocess blocks indefinitely; the result is indistinguishable from "nothing to do."

**Root cause:** `SynthesisResult` has no error-tracking fields. The pipeline can't distinguish "nothing to synthesize" from "everything failed." This is acceptable while reflect runs manually but becomes an operational blind spot once it runs on a schedule.

## Non-overlapping findings (unique to one pass)

**From code review only:**
- **disagree-sets-last-reinforced** (high) — disagree resets the decay anchor, partially undoing its own confidence drop. One-line fix, clear correctness bug.
- **watermark-vs-since-mismatch** (medium) — reflect_status uses `completed_at` while pipeline uses `started_at`, with different run-type filters. Misleading status output.
- **feedback-evidence-no-embedding** (medium) — feedback rows permanently invisible to semantic search.
- **8 integration test failures** (high) — 6 search-related, likely a single root cause.

**From architecture only:**
- **reflect.rs is a pass-through** — 64 LOC, 1 symbol, no real orchestration logic. Should be resolved when synthesis.rs is split.
- **DateRange/DisagreeFlagged duplication** in reflect_status with untested aggregation logic.
- **MemoryRow inline construction** at 3 sites with 16 fields each.

## Proposed fix order

### Wave A — Correctness (blocking release)

| Fix | Source | Effort |
|---|---|---|
| Fix disagree `last_reinforced_at` (only set on agree) | review #2 | one line |
| Propagate facets via `Facets::distinct_union` in `emit_consolidated` and `emit_with_supersession` | review #1 / arch #3 | mechanical, infra exists |
| `cargo fmt` on 5 drifted files | verification | trivial |

### Wave B — Test triage (blocking release)

| Fix | Source | Effort |
|---|---|---|
| Triage 8 integration test failures | review #8 | investigate — likely related to wave A fixes + contract drift |

### Wave C — Production safety (ship, fix soon)

| Fix | Source | Effort |
|---|---|---|
| Add `tokio::time::timeout` around LLM calls | review #5 / arch #5 | small |
| Add error-tracking fields to `SynthesisResult` | arch #5 | small |
| Fix watermark column mismatch in reflect_status | review #3 | small |
| Fix hardcoded meta-row type (`mental_model` → source type or dedicated type) | review #4 | small |

### Wave D — Design (follow-up, doesn't block)

| Fix | Source | Effort |
|---|---|---|
| Split synthesis.rs into submodules | arch #1 | medium |
| Resolve reflect.rs pass-through boundary | arch #2 | medium (do alongside synthesis split) |
| Pre-filter contradiction candidates by embedding similarity | review #7 | medium |
| MemoryRow constructor for emission sites | arch #6 | small |
| Extract reflect_status aggregation into testable pure function | arch #4 | small |
| Embed feedback evidence rows (or document + backfill scope) | review #6 | small |

### Slop (batch into any wave)

1. `src/synthesis.rs:302,316` — British spelling "synthesised" vs American conventions elsewhere
2. `src/synthesis.rs:463` — hardcoded `"mental_model"` string should be a named constant
3. `src/llm.rs` — `ClaudeCliLlm::default()` hardcodes model alias `"sonnet"`
4. `src/tools/record_feedback.rs:351` — redundant profile in idempotency key
5. `src/ingest.rs:110-144` — `NARRATION_PREFIXES`/`ANCHOR_PHRASES` may be dead after synthesis pipeline replaced extraction
6. `tests/integration.rs:4340` — identical if/else branches (clippy warning)

## Assessment

The upgrade arc is architecturally sound — facets, consolidated, and llm are well-scoped modules with clean interfaces. The concentration risk is in synthesis.rs, which absorbed all the pipeline complexity. The most consequential bug is the empty-facets emission: it silently breaks tier-1 retrieval for all consolidated content, which is the layer the system exists to produce.

Waves A and B should land before considering this release-ready. Waves C and D are real issues with bounded blast radius that can follow.
