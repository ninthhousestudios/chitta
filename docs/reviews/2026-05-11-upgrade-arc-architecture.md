# Architecture Review: chitta upgrade arc

**Date:** 2026-05-11
**Scope:** Upgrade arc (facets -> reflect pipeline, 141ee8f..503472c)

## Module landscape

The upgrade arc added five modules to chitta's 15-module codebase:

- **facets** (328 LOC, 38 symbols) -- Typed `Facets` struct with SQL clause generation, `HasFacets` trait, and `distinct_union` aggregation. Pure data + query-building; no I/O. Well-tested (11 unit tests). Integrates cleanly with `db::MemoryRow` via trait impl and with search/retrieval via SQL filter generation.

- **consolidated** (179 LOC, 18 symbols) -- Layer classification (`is_consolidated`, `CONSOLIDATED_TYPES`), effective-score computation with half-life decay, and ranking. Replaces the deleted `scoring.rs`. Pure computation, no I/O. Thorough test coverage (11 unit tests). Used by `tools/get_profile.rs` for tier-0 ranking.

- **llm** (136 LOC, 12 symbols) -- Two `Llm` trait implementations: `ClaudeCliLlm` (shells out to `claude` binary) and `ClaudeApiLlm` (direct HTTP to Anthropic API, feature-gated behind `api`). Adapter module -- thin, purpose-clear.

- **synthesis** (1164 LOC, 77 symbols) -- The largest new module by far. Contains the full synthesis pipeline: candidate extraction, clustering, threshold checking, consolidated emission, contradiction detection, supersession, disagree-target extraction, and the orchestrating `run_synthesis` function. 36 unit tests.

- **reflect** (64 LOC, 1 symbol) -- Orchestrator: `reflect_pipeline` fetches raw rows since last run, delegates to `synthesis::run_synthesis`, records the run in `reflect_runs`. Called from the CLI `reflect` subcommand.

Integration with existing modules:

- **db** gained 6 new public functions for the reflect pipeline: `fetch_raw_since`, `fetch_profile_candidates`, `last_synthesis_run`, `insert_reflect_run_with`, `insert_memory_with_derivations`, `get_memory_by_id` (already existed but newly consumed).
- **tools** gained two new tool modules: `reflect_status` (MCP-exposed status tool) and `record_feedback` (agree/disagree feedback loop). The deleted `reflect_summary.rs` was replaced by `reflect_status.rs` (sutra index still carries the ghost).
- **embedding** is consumed by synthesis for consolidated-row embedding at emission time.
- **mcp** gained two new tool registrations (reflect_status, record_feedback).

The deleted `scoring.rs` was cleanly replaced by `consolidated.rs` (same responsibility, narrower scope). The deleted `reflect_summary.rs` was split: its status-reporting half became `reflect_status.rs`, its synthesis half became the `synthesis` + `reflect` module pair.

## Deepening candidates

### 1. synthesis.rs is a god module

- **Files:** `src/synthesis.rs` (1164 LOC, cognitive max 17, cyclomatic max 16)
- **Problem:** `synthesis.rs` contains six distinct responsibilities in a single file: candidate extraction (LLM prompt + parse), clustering (LLM prompt + parse), threshold gating, consolidated emission (embedding + DB write + idempotency key construction), contradiction detection (LLM prompt + parse), and supersession emission. The orchestrator `run_synthesis` (cognitive 17, cyclomatic 16) stitches them all together in one 65-line function with mutable state (`superseded_ids`, `existing`, `result`). Each responsibility has its own struct types, prompts, and parse logic, but they share no trait boundary -- they're just functions in a flat namespace. The test module (570 LOC) is similarly monolithic.

  The deletion test: could you delete `emit_consolidated` without breaking `detect_contradiction`? Yes -- they don't call each other. But they share `Cluster`, `Candidate`, `Llm`, `VALID_TYPES`, and helper functions, creating implicit coupling. Adding a new synthesis phase (e.g., facet inference, which the architectural-deepening PRD mentions) would require touching this already-large file and reasoning about all six concerns at once.

- **Solution:** Extract into submodules under `src/synthesis/`: `extraction.rs` (candidate extraction + parse), `clustering.rs` (cluster + parse), `threshold.rs` (threshold config + check), `emission.rs` (emit_consolidated + emit_with_supersession + idempotency key), `contradiction.rs` (detect + parse), `disagree.rs` (find_disagree_targets). Keep `run_synthesis` in `synthesis/mod.rs` as the orchestrator that composes these pieces. Each submodule owns its types and prompts; shared types (`Candidate`, `Cluster`, `Llm` trait) stay in `mod.rs` or a `types.rs`.

- **Benefits:** Locality -- each LLM interaction (prompt template, response parse, validation) lives in one file. Leverage -- adding facet inference or a new synthesis phase means adding a file, not editing a 1164-line module. The orchestrator becomes a readable pipeline of named steps. Test files split along the same lines, making failure diagnosis faster.

### 2. reflect.rs is a pass-through, not an orchestrator

- **Files:** `src/reflect.rs` (64 LOC, 1 symbol)
- **Problem:** `reflect_pipeline` does five things: (1) capture watermark, (2) fetch last synthesis run from db, (3) fetch raw rows from db, (4) call `synthesis::run_synthesis`, (5) insert reflect run into db. Steps 2, 3, and 5 are direct `db::` calls. Step 4 is a single delegation. The module adds no logic beyond wiring -- it is a shallow pass-through that exists only to give the CLI subcommand something to call.

  Meanwhile, `run_synthesis` in `synthesis.rs` is doing the real orchestration (fetching existing consolidated rows, managing superseded_ids state, iterating clusters). The division between "what reflect manages" and "what synthesis manages" is unclear: reflect handles the temporal window (watermark, since), but synthesis handles the actual stateful iteration over clusters with existing-row fetching. This means the pipeline's DB interaction is split across two modules with no clear seam.

- **Solution:** Either (a) promote `reflect.rs` into a real orchestrator by moving the cluster-iteration loop and existing-row management out of `run_synthesis` and into `reflect_pipeline` (so synthesis exports stateless, composable functions and reflect owns the pipeline state), or (b) fold `reflect_pipeline` into `synthesis.rs` as a convenience entry point and call it directly from main.rs. Option (a) is better if there will be multiple pipelines consuming synthesis pieces (e.g., a "quick reflect" that skips contradiction detection); option (b) is better if reflect is always a single pipeline.

- **Benefits:** Locality -- the full pipeline lifecycle (temporal window, row fetching, synthesis, run recording) lives in one place. Leverage -- clear ownership of pipeline state makes it safe to add pipeline variants (partial reflect, dry-run reflect) without worrying about which module owns the mutable state.

### 3. Facets are default-empty on all synthesis emissions

- **Files:** `src/synthesis.rs` (lines 305, 472 -- `facets: Facets::default()`)
- **Problem:** Both `emit_consolidated` and `emit_with_supersession` set `facets: Facets::default()` on every emitted consolidated row. This means synthesized traits, values, and patterns have no facet annotations -- they won't appear in context-faceted tier-1 search (`search_memories` with `applies_to` filters). The raw source rows that were synthesized *from* do carry facets (the `Facets::distinct_union` function exists precisely to aggregate them), but that information is discarded at emission time.

  This is a silent retrieval gap: a consolidated memory about "Josh prefers deletion over feature flags" that was synthesized from observations tagged `applies_to_domains: ["architecture"]` will not surface when an agent searches with `applies_to: {domains: ["architecture"]}`. The profile tier-0 still works (it ignores facets), but tier-1 is blind to consolidated content.

- **Solution:** At emission time, compute `Facets::distinct_union` over the source rows (which are available via `cluster.source_ids` -> db fetch) and set it on the emitted row. The `Facets::distinct_union` function and the `HasFacets` trait already exist for exactly this purpose. Alternatively, add facet inference as an LLM step (the architectural-deepening PRD mentions this), but the union approach is a zero-LLM-cost baseline that captures most of the signal.

- **Benefits:** Locality -- facet information flows from raw to consolidated layer without a gap, matching the domain model (consolidated memories inherit the context of their sources). Leverage -- every existing faceted search query immediately starts finding consolidated content, improving retrieval without changing the search code.

### 4. Duplicated DateRange/DisagreeFlagged types across tool modules

- **Files:** `src/tools/reflect_status.rs` (lines 38-49), ghost `src/tools/reflect_summary.rs` (lines 37-47)
- **Problem:** `DateRange` and `DisagreeFlagged` are defined identically in `reflect_status.rs` and were previously defined in the now-deleted `reflect_summary.rs`. The code-intel summary flags `DateRange` and `DisagreeFlagged` in `reflect_status.rs` as potentially dead (health score 70). These types are serialization structs used only within the tool handler, but if a new reflect-related tool is added (e.g., a `reflect_history` tool), the types would need to be defined a third time or extracted.

  More broadly, the `reflect_status` handler (cognitive 8, cyclomatic 12) does significant data aggregation inline: iterating rows to compute counts, date ranges, facet summaries, and disagree-flagged lists. This aggregation logic is untested (no unit tests for the handler) and interleaved with DB calls and run recording.

- **Solution:** Extract the aggregation into a pure function: `fn summarize_raw_rows(rows: &[MemoryRow]) -> RawSummary` that returns a struct containing counts, date_range, facet_summary, and disagree_flagged. Move the shared types (`DateRange`, `DisagreeFlagged`) into this summary module. The handler becomes: validate -> fetch rows -> summarize -> record run -> return. The summarize function is trivially unit-testable.

- **Benefits:** Locality -- all raw-row summarization logic lives in one testable function. Leverage -- any future tool that needs to summarize raw rows (reflect history, reflect diff, dashboard) reuses the same code.

### 5. LLM error handling in synthesis is silent-skip with no aggregate signal

- **Files:** `src/synthesis.rs` (lines 58-67 in `extract_candidates`, line 578 in `run_synthesis`)
- **Problem:** When an LLM call fails during extraction, the error is logged with `tracing::warn` and the row is skipped. When all LLM calls fail (network down, CLI not installed, rate limited), `extract_candidates` returns `Ok(vec![])`, `cluster_candidates` returns `Ok(vec![])`, and `run_synthesis` returns `Ok(SynthesisResult { clusters_formed: 0, clusters_emitted: 0, supersessions: 0 })`. This is indistinguishable from a legitimate "no synthesis needed" result. The `reflect_pipeline` logs "nothing to synthesize" only when rows are empty, not when extraction produced zero candidates from non-zero rows.

  The pipeline's `SynthesisResult` has no field for errors, skipped rows, or partial-failure signals. A caller (the CLI subcommand, or a future scheduled job) cannot tell whether synthesis succeeded with nothing to do or failed silently on every row.

- **Solution:** Add error-tracking fields to `SynthesisResult`: `rows_scanned`, `rows_skipped`, `extraction_errors`. Have `extract_candidates` return a richer result `(Vec<Candidate>, usize /* errors */)` instead of silently swallowing failures. The orchestrator propagates these counts through to the run record and the CLI output. If `rows_scanned > 0 && clusters_formed == 0 && extraction_errors > 0`, the caller knows the pipeline degraded.

- **Benefits:** Locality -- error information stays with the pipeline result rather than scattered across log lines. Leverage -- any consumer of the pipeline (CLI, scheduled job, MCP tool) gets actionable diagnostics without parsing logs.

### 6. emit_consolidated constructs MemoryRow structs inline with 16 fields

- **Files:** `src/synthesis.rs` (lines 291-311, 453-477)
- **Problem:** Both `emit_consolidated` and `emit_with_supersession` construct `MemoryRow` structs with all 16 fields spelled out inline. The two construction sites differ only in `memory_type`, `tags`, `content`, and `confidence` -- the remaining 12 fields are either identical boilerplate or trivially derived. When `MemoryRow` gains a new field (which happened multiple times in this arc -- `facets`, `confidence`, `reinforcement_count`, `last_reinforced_at`, `invalidated_at`), every construction site must be updated. There are currently 2 sites in synthesis, plus the one in `record_feedback::insert_evidence_row` that uses raw SQL, making 3 distinct places that build memory rows for insertion.

- **Solution:** Add a builder or constructor on `MemoryRow` (or a `NewConsolidatedRow` struct) that takes the varying fields and fills defaults for the rest. Something like `MemoryRow::new_consolidated(profile, content, memory_type, confidence, embedding, tags, idem_key)` that sets the boilerplate fields once. This is not a new abstraction -- it's removing duplication in struct construction.

- **Benefits:** Locality -- the "what does a new consolidated row look like" question has one answer in one place. Leverage -- adding a field to `MemoryRow` means updating the constructor, not hunting through synthesis and feedback modules.

## Summary

**Highest leverage:** Candidate 1 (split synthesis.rs) and candidate 3 (facets on emissions). Synthesis.rs is the clear priority -- at 1164 LOC with six responsibilities, it is the module most likely to impede future work. The facets gap is the most consequential bug-like finding: it silently degrades retrieval quality for all consolidated content.

**Medium leverage:** Candidate 5 (error visibility) prevents a class of operational blind spots that will matter as soon as reflect runs on a schedule rather than manually. Candidate 6 (row construction) is mechanical but prevents a recurring source of per-field maintenance burden.

**Lower leverage:** Candidates 2 (reflect pass-through) and 4 (duplicated types) are real but less urgent. The reflect/synthesis boundary question (candidate 2) should be resolved as part of candidate 1, since splitting synthesis.rs will force a decision about where the orchestration loop lives.
