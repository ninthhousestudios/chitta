# Code Review: memory_type + type-weighted retrieval (v0.0.3)

**Commits:** `3e6fc62` (feat: add memory_type column) and `df46771` (feat: add type-weighted retrieval scoring)
**Reviewer:** GLM-5.1
**Date:** 2026-04-25

---

## Summary

These two commits introduce a `memory_type` taxonomy for memories and post-fusion type-weighted scoring. The first commit is the heavier lift — a new DB column, migration, validation, and plumbing through every SQL query and tool handler. The second is a thinner follow-on: env-driven weight multipliers applied after retrieval.

Overall: **solid, well-structured work** with thorough contract test updates and consistent patterns. The issues below range from a genuine semantic bug (P1) to smaller style/consistency nits.

---

## P1 — `total_in_profile` count ignores `memory_types` filter

**File:** `src/db.rs:295-302` (in `list_recent_with_count`)

The count query inside the transaction is:

```sql
SELECT count(*)::bigint FROM memories WHERE profile = $1
```

This counts **all** memories in the profile, regardless of the `memory_types` filter applied to the row-fetch query two lines above. The same inconsistency exists in `search_by_embedding` at `src/db.rs:353-366` — the pre-count does filter by `memory_types`, but the `list_recent_with_count` count does not.

**Impact:** A caller filtering `list_recent_memories` by `memory_types=["decision"]` gets back e.g. 3 results but `total_in_profile=5000`. The field is semantically misleading — it looks like it says "3 out of 5000 matched" but it actually means "3 matched your type filter and 5000 exist in the profile regardless." This violates the documented intent that `total_in_profile` reflects the filter scope.

**Fix:** Add the same `AND ($5::text[] = '{}' OR memory_type = ANY($5))` clause to the count query in `list_recent_with_count`, and bind `memory_types` to `$5`.

---

## P2 — Type-weight scoring mutates `similarity` in place, distorting downstream semantics

**Files:** `src/retrieval.rs:129-136`, `src/tools/search.rs:152-160`

Both the hybrid path and the non-hybrid path multiply `hit.similarity` by the type weight and re-sort. The problem: `similarity` is then returned to the caller as-is. A `mental_model` with cosine similarity 0.5 and weight 1.3 will report `similarity=0.65`, which is no longer a cosine similarity — it's a composite score. This conflates retrieval quality with type preference, making it impossible for callers to interpret the score or apply their own `min_similarity` threshold correctly post-hoc.

**Recommendations:**

1. **Rename the field** to `score` (or add a separate `score` field alongside `similarity`) so the semantics are clear. `similarity` stays as pure cosine; `score` is the final retrieval score used for ranking.
2. **Apply type weights before the `min_similarity` filter**, not after. Currently the SQL-level `min_similarity` filter runs on raw cosine, but the post-fusion weight can boost a sub-threshold hit above threshold visually — confusing for anyone comparing `min_similarity` input vs. `similarity` output.
3. **Document the weighting** in the MCP tool description so agents know the score is composite.

---

## P2 — Duplicate type-weight code in two locations

**Files:** `src/retrieval.rs:129-136`, `src/tools/search.rs:152-160`

The exact same weighting + re-sort block appears in both `search_hybrid` and `search.rs::handle`. If the weighting logic changes (e.g. additive instead of multiplicative, or adding a floor), it must be updated in two places.

**Fix:** Extract into a shared function, e.g. `apply_type_weights(hits: &mut [SearchHit], weights: &HashMap<String, f32>)`, and call it from both sites.

---

## P3 — `parse_type_weights` silently ignores malformed entries

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

If a user sets `CHITTA_TYPE_WEIGHTS=mental_model=1.3,badtype=notanumber`, the `badtype` entry is silently dropped. No warning, no error — the system just ignores it. This is especially dangerous because:

- Typos in type names (e.g. `mental_modelz=1.3`) won't raise an error — the weight is just ignored.
- Negative weights or zero could produce surprising behavior (filtering out types entirely) with no warning.

**Fix:** Validate keys against `VALID_MEMORY_TYPES` and warn on unknown keys. Consider warning on weights ≤ 0. At minimum, log the parsed map at startup so operators can verify their config.

---

## P3 — `list_recent` (without count) still accepts `memory_types` but isn't called from any tool

**File:** `src/db.rs:227-252`

The `list_recent` function was updated with the `memory_types` parameter, but only `list_recent_with_count` is used by the tool handler. `list_recent` is currently dead code for the MCP path. This is fine as a general-purpose DB function, but the signature drift should be tracked — if someone adds a caller later, they need to know the filter is available.

No action needed, just noting it.

---

## P3 — `update_memory` error message still says `argument: "content/tags"`

**File:** `src/tools/update.rs:78-79`

```rust
argument: "content/tags".to_string(),
```

The `argument` field still says `"content/tags"` even though `memory_type` and `source` and `metadata` are now valid update targets. This is a minor UX issue — the error message's `argument` field is not self-consistent with the `constraint` field that was updated.

**Fix:** Change to `argument: "updatable_fields".to_string()` or similar.

---

## P3 — No integration test for `memory_type` filtering or type-weighted scoring

The integration tests were updated to include `memory_type: None` / `memory_types: None` in every `StoreArgs` / `SearchArgs` / `ListArgs` construction (mechanical, correct), but no test actually **exercises** the new functionality:

- No test stores a memory with `memory_type: "observation"` and verifies it comes back.
- No test uses `memory_types` filter in search/list and verifies filtering works.
- No test verifies invalid `memory_type` is rejected with the right error.
- No test sets `CHITTA_TYPE_WEIGHTS` and verifies weighted scoring.

The contract tests in `tests/contract.rs` verify wire-shape keys (good), but behavioral coverage is missing. This is the biggest gap in the change set.

**Fix:** Add at minimum:
1. `store_memory_with_non_default_type_returns_type` — store with `memory_type: "observation"`, verify it's returned.
2. `search_memory_types_filter_excludes_non_matching` — store two types, filter by one, verify only matching returned.
3. `invalid_memory_type_rejected` — store with `memory_type: "bogus"`, verify error.
4. `type_weights_boost_ranking` — set weights, verify boosted type outranks unboosted at same cosine.

---

## P4 — Migration `0005` lacks down-migration

**File:** `migrations/0005_memory_type.sql`

There's no corresponding `0005_memory_type.down.sql`. While this is common for additive column migrations, rolling back would leave the column in place. Given this is a `NOT NULL DEFAULT 'memory'` column, a rollback migration that `ALTER TABLE memories DROP COLUMN memory_type` plus dropping the two indexes would be prudent.

---

## P4 — SQL parameter numbering discontinuity in `search_by_embedding`

**File:** `src/db.rs:380-408`

The main ANN query binds parameters as `$1, $2, $3, $4, $5, $6` but `$6` is `memory_types`, which was added after `$5` (fetch_limit). This creates a discontinuity — the parameters go profile($1), query($2), tags($3), min_similarity($4), fetch_limit($5), memory_types($6). The earlier count query in the same transaction uses profile($1), tags($2), memory_types($3). The renumbering is fine for sqlx's positional binding but makes the code harder to audit — someone comparing the two queries in the same function has to mentally remap parameter positions.

Consider adding SQL comments annotating the bind positions, or reordering to put `memory_types` right after `tags` in both queries for consistency.

---

## P4 — `memory_type` validation doesn't check for empty string

**File:** `src/tools/validate.rs:201-212`

The `memory_type` validator checks membership in `VALID_MEMORY_TYPES`, but an empty string `""` will correctly be rejected (not in the list). However, there's no explicit length or character-class validation like other fields have. If a future type is added with unusual characters, the validation could pass unexpected strings.

This is low risk since the type list is closed, but worth noting for when custom types are eventually supported.

---

## P4 — `GetOutput.memory_type` is always serialized (no `skip_serializing_if`)

**File:** `src/tools/get.rs:40`

```rust
pub memory_type: String,
```

This is consistent with how other always-present fields work (`id`, `profile`, etc.), so it's correct. However, some other output structs use `skip_serializing_if = "Option::is_none"` on `source` and `metadata` but not on `memory_type` — which is good, since `memory_type` is never `None`. Just flagging the intentional inconsistency for awareness.

---

## Things Done Well

1. **Systematic column plumbing.** Every SQL query, every `MemoryRow`/`SearchHit` struct, every tool handler output — all updated in one commit. No missed queries, no stale `SELECT *` that would silently miss the new column.

2. **Validation pattern.** `memory_type` (singular) and `memory_types` (plural) follow the same pattern as `tags`/`tags`, making the API consistent. The error messages include the valid type list.

3. **Migration indexes.** Both a single-column index on `memory_type` and a composite index `(profile, memory_type, record_time DESC)` that matches the `list_recent` query pattern — shows awareness of query access patterns.

4. **Idempotent replay safety.** The `memory_type` field is included in the idempotency pre-flight check path correctly — when a replay occurs, the stored `memory_type` is returned as-is, not overwritten.

5. **Contract test coverage.** Every output struct's wire-key set was updated to include `memory_type`, preventing silent wire-shape regressions.

6. **Default value strategy.** `DEFAULT 'memory'` on the column + `unwrap_or_else(|| "memory".to_string())` in the handler means old rows and omitted args both produce the same result — no migration data backfill needed.

---

## Verdict

**Approve with requested changes.** The `total_in_profile` count bug (P1) should be fixed before shipping. The `similarity`/`score` conflation (P2) should be addressed before any downstream agent relies on the numeric value for thresholding. Integration tests for the new feature (P3) are the most impactful addition for confidence.
