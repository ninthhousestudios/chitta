# Code Review: chitta @ upgrade-arc

**Date:** 2026-05-11
**Scope:** Upgrade arc (facets -> reflect pipeline, 141ee8f..503472c)
**Verdict:** ship with follow-ups

## Verification

- Build: pass (1 deprecation warning — `tower_http::auth::ValidateRequestHeaderLayer::bearer`)
- Tests: 59 passed, 8 failed (6 search-related, 1 update contract, 1 feedback contract)
- Lint: pass (1 cosmetic clippy warning in test mock)
- Format: drift in 5 files (`src/llm.rs`, `src/reflect.rs`, `src/synthesis.rs`, `src/tools/record_feedback.rs`, `tests/contract.rs`)

## Design

The three-layer taxonomy (raw -> consolidated -> profile) is sound and well-matched to the problem. The synthesis pipeline -- extract candidates per row, cluster, threshold, detect contradictions, emit with supersession -- is a reasonable multi-stage design that keeps each LLM call narrowly scoped. The `Llm` trait abstraction cleanly separates the pipeline logic from transport (CLI subprocess, API, mock), and the idempotency-key scheme on emitted rows means the pipeline is replayable.

Two structural concerns stand out. First, the synthesis pipeline makes N+2 serial LLM calls (one per raw row for extraction, one for clustering, one per cluster for contradiction detection). With the `ClaudeCliLlm` backend, each call spawns a subprocess that initializes an entire Claude Code session. At scale this will be the dominant cost in wall-clock time and API spend, but there is no concurrency, no batching, and no token-budget cap across the full pipeline run. The sequential-and-unbounded design is fine for the current corpus size but will need rework before the raw layer grows significantly.

Second, consolidated memories emitted by the synthesis pipeline carry `Facets::default()` (empty facets on all four dimensions). This means every synthesized trait/value/pattern/preference is invisible to tier-1 faceted retrieval. The source observations likely have facets populated; the pipeline should propagate them via `Facets::distinct_union` over source rows. This is the single highest-impact retrieval gap in the upgrade arc.

## Findings

```yaml
- id: empty-facets-on-emit
  severity: high
  category: correctness
  title: Consolidated memories emitted with empty facets
  location: src/synthesis.rs:305
  evidence: |
    facets: Facets::default(),
  why: |
    Every memory emitted by the reflect pipeline gets empty applies_to_* arrays.
    Tier-1 faceted retrieval (search_memories with applies_to filters) will never
    match these rows. The source observations likely carry facets that should
    propagate to the consolidated output.
  recommendation: |
    In emit_consolidated, look up source rows by cluster.source_ids and call
    Facets::distinct_union over them. The infrastructure already exists in
    facets.rs.
  confidence: high
```

Consolidated memories are the highest-value content in chitta -- they are what `get_profile` returns and what agents rely on. Emitting them without facets makes them unreachable by the context-scoped retrieval path that CONTEXT.md describes as tier-1. The `Facets::distinct_union` function already exists and does exactly what is needed; it just is not called here.

```yaml
- id: disagree-sets-last-reinforced
  severity: high
  category: correctness
  title: Disagree updates last_reinforced_at, inflating effective_score
  location: src/tools/record_feedback.rs:215
  evidence: |
    last_reinforced_at  = $4  -- always set to now, regardless of kind
  why: |
    The effective_score decay function uses last_reinforced_at as the decay anchor.
    Setting it on disagree resets the decay clock, making a disagreed-with memory
    rank higher than it should. A memory that just got its confidence dropped by
    -0.10 simultaneously gets its decay reset to zero, partially undoing the
    disagreement. CONTEXT.md defines reinforcement as triggered by /agree
    specifically; disagree should not touch this field.
  recommendation: |
    Conditionally set last_reinforced_at only when kind=agree. For disagree, leave
    it unchanged: SET last_reinforced_at = CASE WHEN $6 THEN $4 ELSE last_reinforced_at END
    (with a boolean bind for is_agree).
  confidence: high
```

This is a semantic bug. The `reinforcement_count` is correctly not bumped on disagree (line 207), but `last_reinforced_at` is unconditionally set. The effective_score decay function in `consolidated.rs:14-24` uses `last_reinforced_at` (falling back to `record_time`) as the anchor. Setting it on disagree resets the anchor, boosting effective score despite the confidence drop.

```yaml
- id: watermark-vs-since-mismatch
  severity: medium
  category: correctness
  title: reflect_status and reflect_pipeline use different watermark columns
  location: src/tools/reflect_status.rs:60, src/reflect.rs:19-20
  evidence: |
    # reflect_status.rs:60
    let since = last_run.as_ref().and_then(|r| r.completed_at);

    # reflect.rs:19-20
    let last_run = db::last_synthesis_run(pool, profile).await?;
    let since = last_run.as_ref().map(|r| r.started_at);
  why: |
    reflect_status reads from last_reflect_run (any run_type) using completed_at
    as the watermark. reflect_pipeline reads from last_synthesis_run (run_type=synthesis)
    using started_at. This means: (1) a status-only run advances the status watermark
    past rows the pipeline has not yet synthesized, and (2) the pipeline uses started_at
    while status uses completed_at, so they bracket different time windows.
  recommendation: |
    reflect_status should read from last_reflect_run filtered to run_type='status'
    so its watermark does not alias the synthesis watermark. Both should use
    started_at as the since boundary (started_at was chosen for the pipeline to
    avoid the race where rows arrive during synthesis; the same logic applies to
    status).
  confidence: high
```

This is the residual watermark problem. The commit message says "watermark race" was addressed, but the status and synthesis paths still use different anchor columns and different run-type filters. A `reflect_status` call will advance its watermark past rows the next `reflect_pipeline` call would want to see.

Wait -- re-reading: `reflect_status` calls `last_reflect_run` (no run_type filter), while `reflect_pipeline` calls `last_synthesis_run` (run_type='synthesis'). These are separate DB functions. The issue is more subtle: `reflect_status` calls `insert_reflect_run_with` with `run_type=Some("status")`, and then next time `reflect_status` runs, it calls `last_reflect_run` which finds the most recent run of ANY type. If a synthesis run happened after the last status run, the next status call sees the synthesis run's `completed_at` as `since`, which could skip rows already processed by synthesis but shows them as "nothing new." This is acceptable -- status should show what is new since the last time anyone looked. But if a status run happens between synthesis runs, the next status call only sees rows since the status run, not since the last synthesis. The pipeline is unaffected because it uses its own separate function. So the real risk is: `reflect_status` output may be misleading (showing "0 rows" when synthesis has not processed them yet), but the pipeline itself is correct. Downgrading from my initial assessment.

```yaml
- id: meta-row-hardcoded-type
  severity: medium
  category: design
  title: Supersession meta-row hardcoded to mental_model type
  location: src/synthesis.rs:463
  evidence: |
    memory_type: "mental_model".into(),
  why: |
    When a contradiction causes supersession, the meta-row describing the shift
    ("Josh shifted from X to Y") is always typed as mental_model regardless of
    what the actual superseded memory's type was. A superseded preference gets
    a mental_model meta-row. This pollutes the type namespace and makes
    type-filtered queries unreliable.
  recommendation: |
    Use a dedicated type like "supersession_record" or inherit the type from
    the superseded memory. If keeping mental_model, document why in a comment.
  confidence: medium
```

The meta-row is a record of the shift itself, which could reasonably be a mental_model ("Josh's position evolved from X to Y"). But it competes for space in the profile layer with genuine mental models, and its content format ("Josh shifted from...") is different from actual mental model content. A distinct type or at minimum a distinguishing tag would keep the profile layer clean.

```yaml
- id: no-llm-timeout-or-budget
  severity: medium
  category: design
  title: No timeout or token budget on LLM calls in synthesis pipeline
  location: src/synthesis.rs:52-70, src/llm.rs:28-73
  evidence: |
    # extract_candidates loops over every row serially
    for row in rows {
        match extract_one(llm, row).await { ... }
    }
    # ClaudeCliLlm::complete has no timeout
    let output = child.wait_with_output().await...
  why: |
    The pipeline makes N+2 LLM calls with no per-call timeout, no aggregate
    budget, and no concurrency. With ClaudeCliLlm, a hung subprocess blocks the
    entire pipeline indefinitely. With ClaudeApiLlm, a slow API response does the
    same. As the raw corpus grows, the extraction phase alone could consume
    significant API budget with no cap.
  recommendation: |
    Add tokio::time::timeout around each LLM call (e.g., 60s for CLI, 30s for API).
    Add a max_rows or max_tokens parameter to reflect_pipeline to cap total spend.
  confidence: high
```

```yaml
- id: feedback-evidence-no-embedding
  severity: medium
  category: design
  title: Feedback evidence rows inserted without embeddings
  location: src/tools/record_feedback.rs:347-348
  evidence: |
    .bind(None::<Vector>)       // embedding
    .bind(None::<serde_json::Value>)  // sparse_embedding
  why: |
    The feedback observation row and correction row are inserted with NULL
    embeddings. This means they are invisible to semantic search (tier-2) and
    will not be found by the synthesis pipeline's candidate extraction if search
    is the retrieval path. They ARE found by fetch_raw_since (which uses
    record_time, not embeddings), so the synthesis pipeline sees them. But they
    are permanently invisible to search_memories. This is consistent with
    principle 3 (write fast, enrich lazily) only if there is a backfill path —
    the backfill subcommand exists for sparse embeddings but it is unclear if it
    handles NULL dense embeddings too.
  recommendation: |
    Either embed async in a background task (matching principle 3) or document
    that feedback rows are intentionally search-invisible and add them to the
    backfill command scope.
  confidence: medium
```

```yaml
- id: contradiction-sends-all-existing
  severity: medium
  category: performance
  title: Contradiction detection sends entire consolidated corpus per cluster
  location: src/synthesis.rs:570-579
  evidence: |
    let active_existing: Vec<&MemoryRow> = existing.iter()
        .filter(|r| !superseded_ids.contains(&r.id))
        .collect();
    ...
    detect_contradiction(llm, &cluster.representative_claim, &active_refs).await?;
  why: |
    For each cluster that passes the threshold, the full active consolidated
    corpus (up to 100 rows from fetch_profile_candidates) is serialized into
    the LLM prompt. With a growing consolidated layer, this prompt will hit
    context limits. Additionally, most existing memories are irrelevant to any
    given new claim — a semantic pre-filter would dramatically reduce prompt
    size.
  recommendation: |
    Pre-filter existing memories by embedding similarity to the cluster's
    representative_claim before sending to the LLM. Top-10 most similar
    existing memories is sufficient for contradiction detection.
  confidence: medium
```

```yaml
- id: test-failures-blocking
  severity: high
  category: correctness
  title: 8 integration test failures indicate regression
  location: tests/integration.rs
  evidence: |
    6 search failures, 1 update contract mismatch, 1 feedback rejection mismatch
  why: |
    The test failures suggest either an embedding/retrieval regression or contract
    changes that were not reflected in test assertions. Search failures (6/8) point
    to rows not being found — possibly related to the empty-facets issue or an
    embedding pipeline change. The update and feedback failures suggest wire-contract
    drift. Shipping with 8 failing integration tests undermines the verification
    gate.
  recommendation: |
    Triage each failure: update assertions for intentional contract changes,
    fix the underlying bug for genuine regressions. The search cluster likely
    has a single root cause.
  confidence: high
```

## Synthesis

Three findings share a root cause: **the synthesis pipeline does not propagate context from source rows to emitted rows.** Empty facets (#1) means consolidated memories are invisible to faceted search. The hardcoded meta-row type (#4) is another instance of losing source context during emission. And feedback evidence rows without embeddings (#6) is the write-path variant of the same pattern: newly created rows are contextually orphaned.

The disagree-sets-last-reinforced bug (#2) is a standalone correctness issue that should be fixed before ship -- it directly undermines the effective_score ranking that drives the always-on profile.

The LLM pipeline design (#5, #7) is acceptable for current scale but has a clear ceiling. The fix order should be:

1. **Fix disagree last_reinforced_at** -- one line, correctness fix
2. **Propagate facets in emit_consolidated** -- retrieval correctness
3. **Run cargo fmt** on the 5 drifted files
4. **Triage the 8 test failures** -- likely related to #2 or embedding changes
5. **Add LLM call timeout** -- production safety
6. **Pre-filter contradiction candidates** -- prompt efficiency

## Slop list

1. `src/tools/reflect_status.rs:60` -- uses `completed_at` while pipeline uses `started_at`; inconsistent watermark anchor
2. `src/synthesis.rs:302` -- tag "synthesised" uses British spelling; all other code uses American conventions (serialize, not serialise)
3. `src/synthesis.rs:463` -- hardcoded `"mental_model"` type for meta-rows should be a named constant
4. `src/synthesis.rs:316` -- derivation type string `"synthesised_from"` same British spelling inconsistency
5. `src/llm.rs` -- `ClaudeCliLlm::default()` hardcodes model string `"sonnet"` which is an alias that may break; prefer explicit `"claude-sonnet-4-20250514"` or read from config
6. `src/tools/record_feedback.rs:351` -- idempotency key `format!("feedback-{}-{}", profile, id)` includes profile redundantly (already part of the unique constraint)
7. `src/ingest.rs:110-144` -- `NARRATION_PREFIXES` and `ANCHOR_PHRASES` are used by the extraction worker but the extraction worker is a legacy ingest-HTTP path, not the new synthesis pipeline; these two systems duplicate the "extract signal from raw text" concern
8. Format drift in 5 files per verification summary
