# 2026-05-08 — chitta architecture spike (vidhi-architecture)

## Scope

Checkpoint review during the working-model pivot (commits `wm-1` … `wm-12`, 41 files, +4226/-369). Goal: surface deepening candidates that would improve testability and AI-navigability — not a release gate, not a code-review pass.

I read `vidhi-architecture/SKILL.md` and `LANGUAGE.md` for vocabulary, `chitta/CONTEXT.md` for domain terms, ADR 0001 (working-model framing — not relitigated below), the README, and the code-intel summary at `/tmp/review-pack-chitta-2026-05-08/20-code-intel/SUMMARY.md`. Tooling: `sutra_map`, `sutra_outline`, and direct file reads against `tools/{search, store, supersede, get_profile, reflect_summary}.rs`, `validators.rs`, `scoring.rs`, `retrieval.rs`, and `db.rs`. I did not run cargo or modify source.

The pivot's new modules — `scoring`, `validators`, the three new tool handlers (`get_profile`, `supersede`, `reflect_summary`), and the migrate binary — are where new domain reasoning has accreted. They are also where I see the clearest deepening opportunities. The hotspots flagged by `sutra_hotspots` (`tools/search.rs`, `embedding.rs`, `retrieval.rs`) are individually deep modules; the friction is between them and the new working-model surface, not inside any one of them.

---

## Deepening candidates

### 1. A `consolidated` module behind which `superseded_by`, `confidence`, and `effective_score` all live

**Files involved:** `src/scoring.rs`, `src/tools/get_profile.rs` (handler `handle`), `src/tools/supersede.rs` (handler `handle`), `src/tools/search.rs::handle` (lines 165–166, 271–280, the `exclude_retired` flag and `apply_type_weights` call), `src/db.rs` (`fetch_profile_candidates`, `supersede_memory`, the `superseded_by` column threaded through `MemoryRow` and every `SearchHit` query, `apply_type_weights` in `retrieval.rs`).

**Problem.** The consolidated layer's behaviours — supersession, reinforcement-decay scoring, type-weight ranking, the consolidated-vs-raw split — are each implemented as a single small function or flag, but they are *coordinated* across five call sites. Consider what a caller has to know to "use the consolidated layer correctly":

- Set `include_raw = false` so `CONSOLIDATED_TYPES` becomes the default filter (`tools/search.rs:189–193`).
- Set `exclude_retired = true` so superseded rows are filtered (`tools/search.rs:166`).
- The `apply_type_weights` re-ranker only fires in the non-hybrid branch in `tools/search.rs::handle`; in the hybrid branch it's already been applied inside `retrieval::search_hybrid` — silently. (See `tools/search.rs:278–280` and `retrieval.rs:147`.)
- `effective_score` is called by `get_profile::handle` but not by `search::handle`; consolidated hits returned from search rank by raw confidence × type-weight, ignoring decay. Two callers, two answers to "what's the score of a consolidated memory?"
- `supersede_memory` writes a `derivation_type='supersedes'` row in `db.rs:supersede_memory`; `tools/supersede::handle` independently checks `superseded_by.is_some()` before delegating. The "is this row already retired?" predicate exists in two places.

`scoring.rs` has the right *idea* (CONTEXT.md says "the decay function and any score composition live in **one app-side module**") but it currently holds only `effective_score`. Type-weight multiplication, the supersession-exclusion predicate, and the consolidated-types list live elsewhere. The interface to "consolidated layer reads" is scattered.

This is also where `sutra_outline` shows the highest cognitive density growth from the pivot: `tools/search.rs::handle` is 26-cognitive, and a meaningful share of that complexity is the consolidated-vs-raw branching, not the dense/RRF retrieval mechanics. `tools/reflect_summary.rs::handle` (cognitive 14, average 14 per the code-intel summary) is doing its own ad-hoc raw-vs-consolidated work via `fetch_raw_since`.

**Solution.** A `consolidated` module (matching the **Layer** term in `CONTEXT.md`) that owns:

- The `CONSOLIDATED_TYPES` constant and the `is_consolidated(memory_type) -> bool` / `is_raw(memory_type) -> bool` predicates (currently a `&[&str]` plus four match arms across files).
- Ranking: a single `rank(hits: &mut [SearchHit], cfg: &SearchConfig, now: DateTime<Utc>)` that applies type-weights *and* decay-via-`effective_score` consistently. Today, `get_profile` uses decay and search does not — that's a behavioural inconsistency hiding in a layout choice.
- Supersession reads: an `active_filter()` predicate or a `fetch_active_consolidated(pool, profile)` query that hides "exclude rows where `superseded_by IS NOT NULL`" — currently a SQL fragment templated into multiple queries in `db.rs` and a boolean flag (`exclude_retired`) propagated through `SearchParams` and `HybridSearchParams`.
- Supersession writes: the "supersede this row" operation as a single call that owns the `already_superseded` precondition + transaction + derivation insert. `tools/supersede::handle` becomes argument validation plus this call.

`scoring.rs` is the seed — rename or fold it into `consolidated` and let the Layer concept be the interface.

**Deletion test.** Delete `consolidated`. Where does the complexity reappear? Five places: the two `unwrap_or(true)` flags in `search::handle`, the `apply_type_weights` call site duplication between hybrid/non-hybrid, the decay computation in `get_profile`, the precondition check in `supersede`, and the SQL filter in `db.rs`. It concentrates rather than disperses. Pass.

**Benefits.**
- *Locality.* The current "consolidated layer" is a convention spread across five files plus a SQL column. Today, if Josh decides retired rows should still surface in tier-2 search with a flag, that change touches `search.rs`, `retrieval.rs`, and `db.rs`. Behind a `consolidated` module it's one place.
- *Leverage.* Callers stop knowing "the consolidated layer is types T1..T5, retired = `superseded_by IS NOT NULL`, ranking = confidence × decay × type-weight" and start calling `consolidated::rank` and `consolidated::active_filter`. The interface — a Layer with predicates, ranking, and supersession — is the abstraction `CONTEXT.md` already names.
- *Tests.* The interesting bugs in the consolidated layer are coordination bugs: "what happens when a retired row gets re-reinforced" or "does decay apply to search hits the same way it applies to profile hits." Today those have to be tested through the search and get_profile handlers, which require Postgres + ONNX. With a `consolidated` module the rank/predicate logic tests at the seam, in-memory.

This module name (`consolidated`) is already in `CONTEXT.md` as **Layer**: "Consolidated / semantic — `trait`, `value`, `pattern`, `preference`, `mental_model`. Confidence-weighted, supersedeable." No new term needed.

---

### 2. A `facets` module owning `applies_to_*` and `RefFilter`

**Files involved:** `src/tools/search.rs::AppliesTo` (struct + 4 `Option<Vec<String>>` fields, 4 `unwrap_or_default` lines, 4 slice references threaded through both retrieval branches), `src/tools/store.rs::StoreArgs` (4 `applies_to_*` fields, 4 `unwrap_or_default` calls, 4 fields on `MemoryRow`), `src/db.rs::SearchParams` (4 `applies_to_*` slice fields, presumably 4 `@>` SQL fragments), `src/retrieval.rs::HybridSearchParams` (4 fields again, mirrored from `SearchParams`), `src/tools/reflect_summary.rs::handle` (3 `BTreeSet`s for distinct domain/skill/project — the situation facet is silently dropped, which may be a real bug).

**Problem.** The four facets defined in `CONTEXT.md` — `applies_to_domains`, `applies_to_skills`, `applies_to_projects`, `applies_to_situations` — are cited as a single domain concept ("**Facet**: a column on a memory that scopes its retrieval relevance"), but in code they are four parallel scalars repeated everywhere. Counting:

- `StoreArgs` declares 4 fields and unwraps 4 times (`store.rs:54–64`, `160–163`).
- `MemoryRow` carries 4 fields.
- `SearchArgs::AppliesTo` is a struct of 4 `Option<Vec<String>>`; `search::handle` then unwraps each (`search.rs:216–219`).
- `SearchParams` and `HybridSearchParams` each repeat the same 4 slice fields (`db.rs:77–93` and `retrieval.rs:14–31`).
- `reflect_summary::handle` aggregates 3 of the 4 — the situation facet is silently absent. Either a bug or an oversight; either way it's a tell that the facet concept isn't a module yet.
- `CONTEXT.md` warns: *"Adding a fifth facet requires a migration (deliberate friction)."* The migration is the easy part — the four-fold code duplication is what makes a fifth facet expensive.

The deletion test on a hypothetical `Facets` struct is the test: today the facets aren't a module, just a naming convention with `applies_to_` as the prefix. Deleting "Facets" today doesn't change anything because there's nothing to delete; the question is whether *introducing* one would concentrate.

**Solution.** A `facets` module with:

- A `Facets` struct (or `FacetSet`) holding all four vocabularies. Used by `MemoryRow`, `StoreArgs`, `SearchArgs::AppliesTo`, `SearchParams`, `HybridSearchParams`, and the `reflect_summary` aggregator.
- A facet-filter SQL builder so the four `@>` clauses are emitted from one place — adding a fifth facet becomes "add a field to `Facets` + one match arm" instead of a six-file change.
- An iterator over (facet_name, values) pairs so `reflect_summary` aggregates all facets uniformly and stops dropping `situations`.

I'd locate it next to `validators.rs` at the crate root, since facets are a domain concept used by both tools and storage.

**Deletion test.** Delete `facets`. Where does complexity reappear? In six locations: the four field clusters in `StoreArgs`/`MemoryRow`/`SearchParams`/`HybridSearchParams`, the `AppliesTo` unwrap dance, and the partial aggregation in `reflect_summary`. The fifth-facet migration cost reappears immediately. Concentrates. Pass.

**Benefits.**
- *Locality.* The "what is a facet?" question has one answer in the code, matching its one definition in `CONTEXT.md`. Adding `applies_to_<X>` is one struct field plus a SQL fragment, not six near-identical edits.
- *Leverage.* Every consumer (store, search, reflect, retrieval) gets uniform facet handling. The `reflect_summary` situation-bug doesn't recur because the aggregator iterates the `Facets` shape rather than naming three of four.
- *Tests.* The facet-filter SQL builder is testable in isolation against fixture inputs — today its behaviour is only observable through the search handler with a live database.

The deepened module's name is `facets`, matching `CONTEXT.md`'s **Facet**. No new term.

---

### 3. (Mediocre — listed for completeness, would not pursue) An `embedding_io` module wrapping `Embedder` + sparse-JSON serialization

`tools/store.rs::handle` does the `embed_full` call, then inline JSON-serializes the sparse map (`store.rs:135–142`); `tools/update.rs` (cognitive 11, per code-intel) almost certainly does the same. This is a modest dedup with one extra adapter point, but the actual logic is two lines and the `Embedder` interface is already deep. The deletion test fails — deleting an `embedding_io` wrapper concentrates one `serde_json::to_value` call into a function, which is *moving* complexity, not concentrating it. **Not a real candidate.** Cite it only as evidence the embedder's interface is already at the right depth.

---

## What I'd grill on first if Josh were here

I would grill candidate (1) hardest, because there's a behavioural smell hiding in it that I'm not certain about: **search hits and profile hits rank consolidated memories by different formulas today** (search omits decay; `get_profile` includes it). That might be intentional — search has its own recency boost via `recency_weight` and `recency_half_life_days` in `SearchConfig`, and double-applying decay would compound — or it might be drift. Either way, the fact that I had to read four files to figure out which formula runs where is the deepening signal. Before sketching an interface for `consolidated`, I'd want Josh to tell me whether search results *should* honour `effective_score` decay and whether `apply_type_weights` belongs in the consolidated module at all (it's arguably retrieval-level config, not a property of the layer). The shape of `consolidated::rank` depends on that answer. Candidate (2) is more mechanical — the question there isn't whether to do it but whether `Facets` should be one struct or a typed enum with per-facet validation.
