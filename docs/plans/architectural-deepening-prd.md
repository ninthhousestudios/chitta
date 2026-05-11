# PRD — architectural deepening: facets + consolidated modules

Status: draft, pending Josh approval
Date: 2026-05-09
Yojana task: chitta/35
Parent PRD: `docs/plans/working-model-prd.md` (chitta/13)
Glossary: `CONTEXT.md`

## Problem Statement

Two cross-cutting concepts — facets and consolidated-layer operations — are spread across 6+ files as parallel scalars and ad-hoc checks. Adding a 5th facet requires editing six structs. The consolidated-layer concept (types, scoring, active filter, supersession) has no single owner — it's scattered across `scoring.rs`, `tools/search.rs`, `tools/get_profile.rs`, `tools/supersede.rs`, `retrieval.rs`, and `db.rs`. The search and profile retrieval paths use different ranking strategies with no shared vocabulary for why.

This creates friction for every downstream task (extraction pipeline, feedback tool, future facets) and makes the codebase harder to reason about.

## Solution

Two new modules, landed in two waves:

1. **`src/facets.rs`** — a single `Facets` struct replacing 6 duplicated 4-scalar groups.
2. **`src/consolidated.rs`** — single owner of consolidated-layer types, scoring, active filtering, and ranking.

## Design Decisions

### D1: Search ranking uses effective_score for consolidated hits

Search uses a two-stage pipeline: recall by embedding similarity, then rank by effective_score (`confidence × decay(last_reinforced_at)`). Low-confidence hits still appear, just ranked lower. This makes effective_score the universal "how much do we believe this right now" measure across both `get_profile` and `search_memories`.

Rejected alternatives:
- Keep search and profile scoring completely independent
- Blend confidence into the similarity score during RRF fusion (opaque, mixes orthogonal signals)

### D2: Ranking is layer-aware

Effective_score ranks consolidated hits. Recency_weight ranks raw hits. Each layer gets the ranking signal natural to it — raw observations are recent-is-better, consolidated claims are confidence-is-trust. No arbitrary default confidence on raw rows.

### D3: type_weights stays in retrieval

`type_weights` is a search-time tuning knob, not an intrinsic property of the consolidated layer. The consolidated module owns the domain concept (types, active filter, effective_score); retrieval owns search-specific ranking concerns (type_weights multiplier applied after effective_score).

### D4: Facets struct is owned, always-populated

Single `Facets` struct with `Vec<String>` × 4. Empty vec = no facets. Wire types (`StoreArgs`, `SearchArgs`) keep `Option<Vec<String>>` serde fields but convert to `Facets` at the boundary. Borrowed contexts take `&Facets` instead of four separate `&[String]` params. Aggregation (`distinct_union`) is a collection-level function, not a method on the struct.

## Wave 1: Facets module

Pure refactor — behavior-preserving.

### What to build

A new `src/facets.rs` module containing:

```rust
pub struct Facets {
    pub domains: Vec<String>,
    pub skills: Vec<String>,
    pub projects: Vec<String>,
    pub situations: Vec<String>,
}
```

### Changes

1. **Introduce `Facets` struct** with `Default` (all empty vecs), `From<AppliesTo>` (None → empty vec), `is_empty()`.

2. **Replace parallel scalars in `MemoryRow`** — the four `applies_to_*` fields become `pub facets: Facets`. Update `FromRow` derivation (may need a manual impl or a flatten pattern for sqlx).

3. **Replace parallel scalars in `SearchParams`** — the four `applies_to_*: &[String]` become `pub facets: &'a Facets`.

4. **Replace parallel scalars in `HybridSearchParams`** — same as SearchParams.

5. **Replace `ProfileEntry`** — four fields become `pub facets: Facets`.

6. **Replace `AppliesTo` in search.rs** — either delete and embed `Facets` directly in `SearchArgs` with serde defaults, or keep as a thin wire-format struct with `Into<Facets>`.

7. **Add `Facets::sql_contains_filters()`** — replaces the 4 parallel `if !empty { AND applies_to_X @> $N }` blocks in `db::search_by_embedding`.

8. **Add `Facets::distinct_union(rows: &[impl HasFacets]) -> Facets`** — replaces the 4 parallel `BTreeSet` accumulation loops in `reflect_status::handle`.

9. **Delete the four `distinct_*` fields from `ReflectStatusOutput`**, replace with `pub facet_summary: Facets`.

### Acceptance criteria

- All existing tests pass with no behavior change
- `Facets` struct is the single source of truth for the four facet columns
- Adding a hypothetical 5th facet requires changing one struct + one migration, not six edits
- Deletion test: removing `src/facets.rs` concentrates rather than disperses complexity

### Files touched

- `src/facets.rs` (new)
- `src/db.rs` — `MemoryRow`, `SearchParams`, `search_by_embedding`, `fetch_profile_candidates`
- `src/retrieval.rs` — `HybridSearchParams`, `search_hybrid`
- `src/tools/search.rs` — `AppliesTo`, `SearchArgs`, `handle`
- `src/tools/get_profile.rs` — `ProfileEntry`
- `src/tools/reflect_status.rs` — `ReflectStatusOutput`, `handle`
- `src/tools/store.rs` — wherever StoreArgs maps to MemoryRow
- `src/tools/list.rs` — if it exposes facets in output
- `src/lib.rs` — `pub mod facets`
- `tests/integration.rs`, `tests/contract.rs` — update struct construction

## Wave 2: Consolidated module

Behavior change — search ranking becomes layer-aware.

### What to build

A new `src/consolidated.rs` module containing:

1. **`CONSOLIDATED_TYPES: &[&str]`** — moved from `search.rs:63`. Single source of truth.

2. **`is_consolidated(memory_type: &str) -> bool`** — replaces ad-hoc `CONSOLIDATED_TYPES.contains()` checks in supersede.rs and search.rs.

3. **`effective_score(confidence, last_reinforced_at, record_time, now) -> f32`** — moved from `scoring.rs`. The decay function lives here.

4. **`rank(rows: Vec<MemoryRow>, now: DateTime<Utc>) -> Vec<(f32, MemoryRow)>`** — compute effective_score for each row, sort descending. Used by both get_profile (then truncate to 30) and search (for consolidated hits).

5. **`is_active(row: &MemoryRow) -> bool`** — `superseded_by.is_none() && invalidated_at.is_none()`. Documents the predicate in one place. (The SQL still has its own WHERE clause for performance, but the app-side check exists for post-fetch filtering.)

### Changes

1. **Introduce `src/consolidated.rs`** with the five items above plus the existing tests from `scoring.rs`.

2. **Delete `src/scoring.rs`** — all content moves to consolidated.rs.

3. **Update `get_profile::handle`** — replace inline score-sort-truncate with `consolidated::rank()` then `.truncate(PROFILE_LIMIT)`.

4. **Update `search::handle`** — after RRF fusion and type_weights:
   - Partition hits into consolidated vs raw (using `consolidated::is_consolidated`)
   - Rank consolidated hits by effective_score (secondary sort after similarity recall)
   - Rank raw hits by existing recency_weight
   - Interleave or concatenate (consolidated first, then raw — or by final score; needs a call)

5. **Update `supersede::handle`** — replace inline type check with `consolidated::is_consolidated()`.

6. **Update `db.rs`** — `fetch_profile_candidates` references `consolidated::CONSOLIDATED_TYPES` instead of hardcoding the type list in SQL (or: keep the SQL literal for performance, but add a compile-time assertion that it matches `CONSOLIDATED_TYPES`).

### Open question for implementation

How should consolidated and raw hits interleave in search results? Options:
- **Consolidated first, then raw** — simple, clear separation
- **Merged by a composite score** — more integrated, but needs a formula to compare effective_score with recency-boosted similarity

Recommendation: consolidated first, then raw. The consumer knows what they're looking at. A `layer: "consolidated" | "raw"` field on each hit makes it explicit.

### Acceptance criteria

- `scoring.rs` is deleted; `consolidated.rs` owns effective_score and its tests
- `CONSOLIDATED_TYPES` is defined in one place
- `get_profile` and `search` both call `consolidated::rank()` for consolidated hits
- Search results include a `layer` field indicating consolidated vs raw
- Consolidated hits in search are ordered by effective_score within the similarity-recalled set
- Raw hits (when `include_raw=true`) are ordered by recency_weight as before
- All existing tests pass; new test verifies layer-aware ordering

### Files touched

- `src/consolidated.rs` (new)
- `src/scoring.rs` (deleted)
- `src/tools/search.rs` — `CONSOLIDATED_TYPES` removed, handle updated
- `src/tools/get_profile.rs` — handle simplified
- `src/tools/supersede.rs` — type check updated
- `src/retrieval.rs` — may need to expose the partition point
- `src/db.rs` — `fetch_profile_candidates` type list
- `src/lib.rs` — `pub mod consolidated`, remove `pub mod scoring`
- `tests/integration.rs` — new layer-aware ordering tests

## Sequencing

Wave 1 (facets) lands first. Wave 2 (consolidated) depends on it because `MemoryRow` and `SearchParams` will carry `Facets` by then, and consolidated functions should take `&Facets` where needed.

Both waves are AFK-sliceable.
