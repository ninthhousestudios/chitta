# Code Review: memory_type + Type-Weighted Retrieval (v0.0.3)

**Commits:** `3e6fc62` (feat: add memory_type column) and `df46771` (feat: add type-weighted retrieval scoring)  
**Reviewer:** Kimi-K2.6  
**Date:** 2026-04-25  
**Project:** chitta-rs v0.0.2 -> v0.0.3

---

## Executive Summary

These two commits collaboratively implement the "typed memories" foundation for the v0.0.3 cognitive confluence architecture. Commit `3e6fc62` is the structural backbone — a new `memory_type` column, migration, validation rules, and plumbing through every SQL query, tool handler, and output struct. Commit `df46771` is the behavioral layer — post-retrieval scoring multipliers (`CHITTA_TYPE_WEIGHTS`) applied after hybrid/dense ranking.

The implementation is **mechanically sound** with no compilation errors, all 37 lib tests pass, and the contract-test updates are thorough. However, there are **three genuine issues** that affect correctness or observability, plus several design/DRY concerns that will create maintenance burden if left unaddressed.

---

## P1 — `total_in_profile` count in `list_recent_with_count` is missing BOTH `tags` AND `memory_types` filters

**File:** `src/db.rs:295-302`

```rust
let count: i64 = sqlx::query_scalar(
    r#"
    SELECT count(*)::bigint FROM memories WHERE profile = $1
    "#,
)
.bind(profile)
.fetch_one(&mut *tx)
.await?;
```

This count query ignores **both** the `tags` filter (pre-existing) and the new `memory_types` filter. A caller who requests `list_recent_memories(profile="x", tags=["rust"], memory_types=["decision"])` will receive 3 results but `total_in_profile=5000` (the entire profile size). This makes the `total_in_profile` field semantically useless for any filtered query.

**Impact:** High for any UI or agent that uses `total_in_profile` to reason about result completeness or pagination. The value is actively misleading.

**Fix:** Add the same `tags && $N` and `memory_type = ANY($M)` clauses to the count query, binding the same parameter slices.

---

## P1 — `query_log` schema silently drops `memory_types` and `type_weights`, making replay inaccurate

**Files:** `migrations/0002_query_log.sql`, `src/db.rs:588-607` (`insert_query_log`), `src/main.rs:183-193` (`run_replay`)

The `query_log` table does not store:
- `memory_types` — which filter leg was active during the search
- `type_weights` — which scoring bias was applied

When `run_replay` re-executes a logged query, it hardcodes `&[]` for `memory_types` and uses whatever `type_weights` are in the current environment (which may differ from the original run):

```rust
db::search_by_embedding(
    &pool,
    &entry.profile,
    &entry.embedding,
    entry.k as i64,
    &entry.tags,
    &[],              // <-- always ignores original memory_types filter
    entry.min_similarity,
    0.0,              // <-- ignores original recency_weight
    30.0,
)
```

**Impact:** High for retrieval research. A regression sweep that uses `memory_types` or `type_weights` will produce replay overlap numbers that are measuring something different from the original query. This undermines the entire purpose of the replay subcommand as a retrieval-evaluation tool.

**Fix:** Add `memory_types text[] NOT NULL DEFAULT '{}'` and `type_weights text` (or a JSON column) to `query_log`, update `insert_query_log` / `read_query_log` / `QueryLogEntry`, and wire them through `run_replay`.

---

## P2 — `similarity` field conflates cosine distance, RRF score, recency boost, and type weight

**Files:** `src/retrieval.rs:117-136`, `src/tools/search.rs:152-160`

The `SearchHit.similarity` field is overloaded with four different semantics depending on the code path:

1. **Dense-only path:** `1.0 - cosine_distance` (a true similarity in [0, 1])
2. **Hybrid (RRF) path:** `1/(k + rank)` sum (a fusion score, not bounded to [0, 1])
3. **After recency weighting:** similarity × `(1 + recency_weight * exp(-age/hl))` (can exceed 1.0)
4. **After type weighting:** similarity × `type_weight` (can be any positive number, or negative if a future caller sets a negative weight)

The value returned to the agent is therefore **uninterpretable** as a thresholding signal. An agent that sees `similarity: 0.65` cannot know whether that means "65% cosine match" or "a mental_model that had 0.5 cosine but got a 1.3x boost."

**Fix:** Either:
- Rename the field to `score` in the output struct (breaking but honest), **or**
- Add a separate `raw_similarity` field alongside `similarity` so agents can threshold on the genuine embedding distance.

Also: validate that `type_weights` values are strictly positive at parse time. A zero or negative weight would silently zero-out or invert rankings.

---

## P2 — Type-weight application is duplicated and has path-dependent ordering

**Files:** `src/retrieval.rs:129-136`, `src/tools/search.rs:152-160`

The exact same block appears in two locations:

```rust
if !search_cfg.type_weights.is_empty() {
    for hit in &mut hits {
        if let Some(&w) = search_cfg.type_weights.get(&hit.memory_type) {
            hit.similarity *= w;
        }
    }
    hits.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal));
}
```

Beyond the DRY violation (which GLM-5.1 noted), the **ordering differs by path**:

- **Hybrid path (`retrieval.rs`):** weights applied after RRF ranking and recency re-ranking, but before the final k-limit is returned to the caller. The candidate pool is exactly k items.
- **Dense path (`search.rs`):** weights applied after SQL `LIMIT` and `dedup_by_field`, but before the token-budget truncation. The candidate pool is `k * dedup_fetch_factor` items (or fewer after dedup).

This means a `mental_model` with a high weight that was dropped by `dedup_by_field` in the dense path **cannot be recovered** by the weighting, whereas in the hybrid path it might have survived RRF and then been boosted. The two paths produce different ranking semantics for the same query.

**Fix:** Extract a shared `apply_type_weights` helper and ensure it runs at the same logical point in both paths (ideally **before** the k-limit and dedup, so weights can rescue high-priority types that would otherwise be truncated).

---

## P2 — `parse_type_weights` silently swallows malformed entries and dangerous values

**File:** `src/config.rs:143-150`

```rust
fn parse_type_weights(s: &str) -> HashMap<String, f32> {
    s.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.trim().to_string(), v.trim().parse::<f32>().ok()?))
        })
        .collect()
}
```

Three issues in one function:

1. **Typos are invisible:** `CHITTA_TYPE_WEIGHTS=mental_modelz=1.3` silently drops the misspelled key. The operator thinks weighting is active; it is not.
2. **Invalid numeric values are invisible:** `mental_model=not_a_number` is dropped silently.
3. **Dangerous values are accepted:** `mental_model=-5.0` or `mental_model=0.0` would invert rankings or zero-out the type entirely. The code does not validate against `VALID_MEMORY_TYPES` or positive value ranges.

**Fix:**
- Validate keys against `VALID_MEMORY_TYPES`.
- Reject non-numeric values with a panic or error at startup (this is a fatal config error, not a runtime warning).
- Clamp or reject weights ≤ 0.
- Log the final parsed map at `INFO` level so operators can verify.

---

## P3 — No unit tests for `memory_type` validation functions

**File:** `src/tools/validate.rs:192-219`

`memory_type` and `memory_types` validation functions have **zero unit test coverage** in the `validate.rs` test module. Every other validation rule (`profile`, `tags`, `k`, `min_similarity`, `max_tokens`, `event_time`, `content_byte_length`) has at least one test. The absence is conspicuous and creates regression risk when the taxonomy expands (e.g., adding a seventh type).

**Fix:** Add a `memory_type_rules` unit test that checks:
- All six valid types pass.
- Empty string, unknown type, and case variations (e.g., `Memory`) fail.
- `memory_types` with a mixed valid/invalid list fails on the first invalid entry.

---

## P3 — No integration test exercises the new feature surface

**File:** `tests/integration.rs`

Every existing integration test was mechanically updated with `memory_type: None` / `memory_types: None` (correct), but **no test actually validates**:

- Storing with a non-default type and retrieving it.
- Filtering search/list by `memory_types`.
- Rejection of an invalid `memory_type`.
- Type-weighted reordering.

This is the largest behavioral coverage gap. The contract tests (`tests/contract.rs`) prevent wire-shape regressions, but they do not exercise logic.

**Fix:** Add four focused integration tests:
1. `store_with_non_default_memory_type_roundtrips`
2. `search_memory_types_filter_excludes_non_matching`
3. `invalid_memory_type_store_rejected`
4. `type_weights_boost_ranking_in_search`

---

## P3 — `update_memory` error message `argument` field is stale

**File:** `src/tools/update.rs:77-79`

```rust
argument: "content/tags".to_string(),
constraint: "at least one of content, tags, source, metadata, or memory_type must be provided",
```

The `argument` field says `content/tags` while the `constraint` lists five fields. This is a minor UX inconsistency in the error contract.

**Fix:** Change `argument` to `"fields"` or `"updatable_fields"`.

---

## P4 — Migration `0005` has no down-migration

**File:** `migrations/0005_memory_type.sql`

Additive column migrations often skip a `.down.sql`, but this column is `NOT NULL` with a default. Rolling back a migration that depends on this column being absent (e.g., for a hot-revert) would require manual `ALTER TABLE memories DROP COLUMN memory_type` and index cleanup.

**Fix:** Add `migrations/0005_memory_type.down.sql` with:
```sql
DROP INDEX IF EXISTS idx_memories_profile_type_record;
DROP INDEX IF EXISTS idx_memories_type;
ALTER TABLE memories DROP COLUMN IF EXISTS memory_type;
```

---

## P4 — SQL parameter numbering in `search_by_embedding` is discontinuous

**File:** `src/db.rs:380-408`

The count query uses `$1, $2, $3` (profile, tags, memory_types). The fetch query uses `$1, $2, $3, $4, $5, $6` with `$6` for `memory_types`, creating a gap where `$4` and `$5` are `min_similarity` and `fetch_limit`. This is functionally correct (sqlx binds by position), but it makes manual audit harder.

**Fix:** Reorder to put `memory_types` at `$4` in both queries, or add inline SQL comments (`-- $1: profile, $2: query_vec, ...`).

---

## P4 — Dense-path recency_weight is passed but not applied in `run_replay`

**File:** `src/main.rs:191`

```rust
0.0,   // recency_weight
30.0,  // recency_half_life_days
```

Replay hardcodes `recency_weight=0.0`, meaning any original query that relied on recency boosting will diverge from its replay. This is pre-existing (not introduced by these commits), but it compounds with the `memory_types` / `type_weights` omissions to make replay progressively less trustworthy as more scoring dimensions are added.

**Recommendation:** Consider whether replay should use the *current* environment's scoring config (to measure "how would today's config have ranked yesterday's queries?") or the *original* config (to measure "did we break anything?"). If the latter, `query_log` needs to store the full scoring context.

---

## Things Done Well

1. **Mechanical thoroughness.** Every SQL `SELECT` list, `INSERT` column list, `WHERE` clause, and `RETURNING` clause was updated. No `SELECT *` footguns. No missed bind parameters.

2. **Backward compatibility via DEFAULT.** The migration uses `NOT NULL DEFAULT 'memory'`, so existing rows and old code paths both produce the same default value without a data backfill.

3. **Idempotent replay safety.** The `(profile, idempotency_key)` pre-flight check path returns the stored `memory_type` correctly — no risk of overwriting on replay.

4. **Consistent API pattern.** `memory_type` / `memory_types` mirrors `tags` / `tags` in naming, validation, and OR-match semantics. The API is predictable.

5. **Index strategy.** The composite `(profile, memory_type, record_time DESC)` index matches the `list_recent` access pattern exactly.

6. **Contract-test discipline.** Every output struct's wire key set was updated to include `memory_type`, which will catch silent serialization regressions.

7. **Version bump discipline.** `Cargo.toml`, `Cargo.lock`, and `mcp.rs` instructions string were all bumped to `0.0.3` in the same commit.

---

## Verdict

**Approve with requested changes.**

The two commits are well-structured and land cleanly, but three issues should be fixed before considering the feature complete:

1. **Fix `list_recent_with_count` to filter the count query by both `tags` and `memory_types`** (P1).
2. **Extend `query_log` schema and replay code to capture `memory_types` and `type_weights`** (P1) — without this, the replay tool is unreliable for the exact features being shipped.
3. **Add integration tests for the new behavioral surface** (P3) — the mechanical updates are correct, but without behavioral tests, future refactors can silently break filtering or weighting.

The `similarity` / `score` semantic conflation (P2) should be addressed before any downstream agent starts thresholding on the numeric value. A simple interim fix is to rename the output field to `score` and add `raw_similarity` as a second field in a follow-up commit.
