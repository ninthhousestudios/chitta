# Code Review: chitta @ HEAD (wm-1..wm-12 checkpoint)

**Date:** 2026-05-08
**Scope:** working-model pivot in progress, 41 files / +4226/-369
**Verdict:** continue with adjustments

The pivot's *shape* is right. There is one likely correctness regression in the dominant hotspot (`min_similarity` is silently dropped on the hybrid path) and one design split that landed half-finished (the new `validators` module owns one contract while the others stayed in `tools/validate.rs`). Both are small, but the first should land before any more wm-* work; the second will only get harder to untangle as more contracts arrive.

## Verification

- Build: pass (1 deprecation warning — `tower_http::auth::...::bearer` at `src/main.rs:470`)
- Clippy: 3 warnings (supersede.rs unwrap-after-is_some; validate.rs collapsible-if; tests/contract.rs unused import)
- Format: drift across multiple files — run `cargo fmt` once before further wm-* commits to keep diffs clean
- Tests: NOT RUN (no live Postgres/ONNX). Treat invariant claims as untested.

## Design

The working-model pivot is a real narrowing, not a relabel. Three concrete things hold it together: (a) `memory_type` is the partition that splits raw from consolidated, and consolidated rows are the only ones that carry `confidence`/`superseded_by`/`reinforcement_count`; (b) `applies_to_*` is the same shape on every row regardless of layer, so retrieval can intersect facets uniformly; (c) `derivations` is the lineage primitive used by both episode-with-sources and supersede-old-with-new. That's a coherent kernel — the eight memory types and the new tools all sit cleanly on top of it.

The new modules earn their depth unevenly. `scoring` is correctly small and pure — one decay function, all the read-time ranking flows through it, exactly the "single place to evolve" the PRD asked for. `tools/get_profile` is the right shape: SQL over-fetches by raw confidence on the partial index, the app applies decay, truncates to 30. `tools/supersede` is a thin contract layer over `db::supersede_memory`; appropriate. `tools/reflect_summary` is a read-only audit/diagnostic helper for the synthesis pipeline (it doesn't *do* synthesis), which is fine but the name reads like it might — see findings.

The shape that doesn't hold is the **validation contract layer**. The PRD calls for a `validators` module that owns decision contracts, episode contracts, memory_type enum, applies_to shape, and idempotency. What landed is `validators.rs` containing one function (`validate_decision_metadata`) plus its tests, while the other contracts (memory_type enum, episode_derivations, ref_filter, external_refs, tags, k, profile, idempotency_key) all stayed in `tools/validate.rs`. The split exists in name only. This is the kind of thing that's cheap to fix at wm-12 and expensive at wm-25, when callers and tests have started picking sides.

The other architectural thing worth flagging: the soft-delete (`invalidated_at`) and supersession (`superseded_by`) state spaces are independent in the schema but the search-side filters treat them symmetrically (`exclude_invalidated` / `exclude_retired`, both default true). That works, but no validator or DB constraint prevents a row from being both superseded *and* soft-deleted, and the supersession path does not check whether the old row has been soft-deleted. That's a state the code can reach but doesn't think about. Probably benign — it just means a soft-deleted row can still be the target of a supersede, which is harmless because both filters hide it — but it's worth a one-line precondition or a CHECK constraint. Foreseeable change risk: if a future leg of retrieval flips one default and not the other, the asymmetry surfaces.

Foreseeable-change check: the system bakes "one profile" (`josh`) into nothing structural — every API takes `profile` and the partial indexes are scoped on it. Good. It bakes "five consolidated types" into one constant (`CONSOLIDATED_TYPES` in `tools/search.rs`) and one literal list in the partial-index DDL. Adding a sixth consolidated type is two edits and a migration; acceptable. The four `applies_to_*` columns are individually-named rather than JSONB, so adding a fifth facet requires a migration — that's the *deliberate friction* the CONTEXT doc names, and the code matches the doc. Good.

## Findings

```yaml
- id: search-min-similarity-dropped-on-hybrid
  severity: high
  category: correctness
  title: search_memories silently ignores min_similarity when hybrid retrieval is enabled
  location: src/tools/search.rs:195-269
  evidence: |
    `min_similarity` is validated, then passed into `db::SearchParams` on the
    non-hybrid branch (line 256). The hybrid branch (`crate::retrieval::search_hybrid`,
    via `HybridSearchParams`) does not receive it — there is no `min_similarity`
    field on `HybridSearchParams`. When `search_cfg.rrf_fts || search_cfg.rrf_sparse`
    is true (the configured production path once sparse lands), a caller that
    asks for `min_similarity: 0.7` gets results with similarity < 0.7 and no error.
  why: |
    Callers use `min_similarity` as a quality gate; silently dropping it returns
    low-confidence material as if it passed the gate. This is the kind of bug
    the gold set will paper over because both legs return *some* results, but it
    will mislead per-tier scoring and any agent that uses the floor as a
    "good enough" threshold.
  recommendation: |
    Plumb `min_similarity` through `HybridSearchParams` and apply it in the
    fusion step (after RRF score is computed but before `LIMIT k`). If the floor
    is meant to apply only to dense, document that explicitly in the SearchArgs
    docstring. I'd plumb it: the floor is a contract, not a leg-specific knob.
  confidence: high
```

The hybrid path is intentional scaffolding (per the pack notes — sparse not fully wired) but the contract leak is real today: the moment `rrf_fts` flips on, the floor disappears.

```yaml
- id: validators-module-split-incomplete
  severity: high
  category: design
  title: validators module owns one contract; the rest stayed in tools/validate.rs
  location: src/validators.rs (whole file) and src/tools/validate.rs (whole file)
  evidence: |
    PRD §"Module layout" lists `chitta::validators` as a new (deep) module
    containing decision/episode contracts, memory_type enum, applies_to shape,
    and idempotency. What landed is `src/validators.rs` containing only
    `validate_decision_metadata` plus tests. The episode-derivations validator,
    memory_type enum, ref_filter, external_refs, tags, k, profile, idempotency_key,
    event_time, content_byte_length, max_tokens, parse_uuid all still live in
    `src/tools/validate.rs`. Two of those (memory_type, episode_derivations)
    are domain contracts that match the PRD's stated home for `validators`.
  why: |
    A module with one function and a 23%-test-dead-ratio (per file_health) is
    not where the rest of the contracts will naturally drift to. The next person
    adding a contract has two homes to choose from and no rule. This is the
    moment to consolidate — every wm-* commit that adds a validator without
    fixing the split makes the merge more painful and tempts the author to
    just add to whichever file is closer.
  recommendation: |
    Move all domain-contract validators (decision metadata, episode derivations,
    memory_type enum, applies_to shape, external_refs typed shape) into
    `src/validators.rs`. Keep `src/tools/validate.rs` as the cross-tool argument
    sanitiser (profile, idempotency_key, k, max_tokens, content_byte_length,
    event_time, tags, parse_uuid) — those are about *call shape*, not *domain*.
    The split passes a "what does this protect?" sniff test if you can rename
    the files: validators.rs = `domain_contracts.rs`, validate.rs =
    `arg_sanitisers.rs`. Today neither is true.
  confidence: high
```

```yaml
- id: supersede-soft-delete-interaction
  severity: medium
  category: correctness
  title: supersede_memory does not check if old_id is soft-deleted; no invariant prevents both states
  location: src/tools/supersede.rs:65-100; src/db.rs:779-807
  evidence: |
    `tools::supersede::handle` checks `old_row.superseded_by.is_some()` and
    rejects, but does not check `old_row.invalidated_at.is_some()`. There is
    no schema CHECK constraint, and `db::supersede_memory` issues an
    UPDATE without a `WHERE invalidated_at IS NULL` guard. A soft-deleted
    row can be marked superseded; a superseded row can be soft-deleted.
  why: |
    Both `exclude_invalidated` and `exclude_retired` default to true in search,
    so the row is hidden either way. The state isn't user-visible today —
    but it's a state the code doesn't model and it leaks into derivations
    (the supersede creates a `derivation_type='supersedes'` row pointing at
    a logically-deleted memory). If a future audit / replay tool reads the
    derivations table, that link is suspect. The cheaper fix is now.
  recommendation: |
    Add an explicit precondition in `tools::supersede::handle`: if
    `old_row.invalidated_at.is_some()`, reject with the standard
    InvalidArgument shape ("memory <id> was soft-deleted at <ts>; restore it
    or pick a different old_id"). Skip the schema constraint — supersession
    is rare enough that an app-level check is fine and keeps the error
    actionable per principle 8.
  confidence: high
```

```yaml
- id: search-retired-naming-mismatch
  severity: medium
  category: contract
  title: argument and field named "retired" but the schema column is "superseded_by"
  location: src/tools/search.rs:94-96 (SearchArgs::exclude_retired)
  evidence: |
    The PRD, CONTEXT.md, and the `db::supersede_memory` function all use
    "superseded". The search argument is `exclude_retired` and the doc comment
    says "superseded by a derivation". One name on the wire, another in the doc.
  why: |
    A caller reading the JsonSchema sees `exclude_retired` and has to map it
    to "superseded_by" mentally; a caller reading CONTEXT.md sees only
    "supersede". This is small now but the wire schema is the agent-facing
    contract — it gets locked in by skill code and slash commands. Worth
    breaking before there are callers.
  recommendation: |
    Rename `exclude_retired` to `exclude_superseded` on SearchArgs. Same for
    any internal helpers. One commit, breaks no DB contract.
  confidence: high
```

```yaml
- id: reflect-summary-name-misleads
  severity: medium
  category: contract
  title: reflect_summary tool name suggests it does synthesis; it only reports counts
  location: src/tools/reflect_summary.rs:13-140
  evidence: |
    The PRD reserves /reflect for the substantive synthesis pipeline (Phase 7b,
    "the hard part where prediction is least reliable"). The new tool here is
    a read-only diagnostic: counts of raw rows since last run, disagree-flagged
    candidates, last-run timestamp. The function is `handle` and the tool name
    is `reflect_summary`. An agent reading the tool list will reasonably
    expect this to *trigger* /reflect, or to summarise the outcome of a
    /reflect run — it does neither.
  why: |
    Skill authors are about to start writing /reflect, /agree, /disagree
    skills against this surface (Phases 7a/8). A misleading name will get
    baked into skill prompts and harness call sites. The agent-native contract
    (principle 4, "errors are instructions") implies tool *names* should also
    be self-describing.
  recommendation: |
    Rename to `reflect_status` or `reflect_preview`. The current behaviour is
    "what would /reflect see right now" — `reflect_status` reads cleanest.
    The actual synthesis tool, when it lands, can take the `reflect` name.
  confidence: medium
```

```yaml
- id: query-log-records-only-dense
  severity: medium
  category: design
  title: query_log captures only the dense embedding; sparse leg is invisible to replay
  location: src/tools/search.rs:322-358
  evidence: |
    The fire-and-forget log writes `embedding: &log_embedding` (dense only).
    `embed_out.sparse` is dropped before the spawn. When hybrid retrieval is
    enabled, replay against the log can reconstruct dense scores but not
    sparse — so RRF fusion can't be reproduced from the log alone.
  why: |
    The PRD calls out query_log as "kept (used for replay/regression eval)".
    Replay that can't reproduce the production fusion isn't replay; it's
    "replay of one leg." Once the gold set lands, this hole becomes visible
    via mismatched scores.
  recommendation: |
    Either (a) add a `sparse_embedding` column to `query_log` and write
    `embed_out.sparse` alongside dense, or (b) document explicitly that
    query_log is a dense-only artifact and that hybrid replay requires
    re-embedding. (a) is the right call — the storage cost is small and the
    truthfulness of the replay loop is load-bearing.
  confidence: medium
```

```yaml
- id: search-truncated-flag-overload
  severity: low
  category: correctness
  title: truncated flag conflates budget-truncation and k-limit; informationally lossy
  location: src/tools/search.rs:306-314
  evidence: |
    `apply_budget` returns `truncated` true when the budget cut results.
    Then a second branch sets `truncated = true` if `results.len() == k`,
    interpreting "we returned exactly k" as "the SQL LIMIT bit." The agent
    can no longer tell which.
  why: |
    A caller that responds to truncation by raising `k` is taking the right
    action when the SQL LIMIT was hit and the wrong action when the token
    budget was the real cap. The flag was supposed to be a single signal but
    is now overloaded.
  recommendation: |
    Either expose two fields (`truncated_by_budget`, `truncated_by_k`) on the
    envelope, or reduce `truncated` to "budget hit" and rely on
    `total_available > results.len()` for the k case (already in the envelope).
    I'd pick the second — fewer fields, the inference is already supported.
  confidence: medium
```

```yaml
- id: store-default-source-orphans
  severity: low
  category: slop
  title: ingest defaults possibly orphaned after serde-default removal
  location: src/ingest.rs:35,38 (default_profile, default_source); src/ingest.rs:109 (NARRATION_PREFIXES)
  evidence: |
    The pack flags these as plausibly dead. ingest.rs cognitive 19 suggests
    the function bodies are dense, but if the `#[serde(default = ...)]`
    attributes that drove them were removed, the fns and consts no longer
    have callers.
  why: |
    Pre-pivot slop. Cheap to delete; deleting ingest defaults you're not using
    also makes the next ingest pass simpler.
  recommendation: |
    Run `cargo +nightly udeps` or do a literal grep for the function names;
    delete if no callers. Fold into the formatting cleanup commit.
  confidence: low
```

```yaml
- id: get-profile-stable-sort
  severity: low
  category: correctness
  title: get_profile sort is not stable on tied effective_score; ordering can flap between calls
  location: src/tools/get_profile.rs:76
  evidence: |
    `sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Equal))` — when two rows
    have identical effective_score (common at confidence=0.70 seed values),
    the relative order depends on the DB return order, which itself depends
    on the partial-index path.
  why: |
    The tier-0 profile is loaded at session start and its order matters for
    which entries fit when the agent's prompt window is tight. Flapping order
    means the same agent gets different "top of mind" between sessions for no
    reason.
  recommendation: |
    Add a tiebreaker: `(effective_score DESC, last_reinforced_at DESC NULLS LAST,
    record_time DESC, id ASC)`. The id tail makes it deterministic.
  confidence: medium
```

```yaml
- id: clippy-supersede-unwrap
  severity: low
  category: slop
  title: unwrap on Option after is_some check
  location: src/tools/supersede.rs:85
  evidence: clippy already flagged this; `old_row.superseded_by.unwrap()` after `if old_row.superseded_by.is_some()`.
  why: trivial; clean up while editing the file for the soft-delete fix.
  recommendation: |
    Use `if let Some(by) = old_row.superseded_by { ... }` to bind once.
  confidence: high
```

```yaml
- id: format-drift-pivot-files
  severity: low
  category: slop
  title: cargo fmt drift across multiple wm-* commits
  location: project-wide (chitta-migrate, scoring, validators, tools/*, tests)
  evidence: cargo fmt --check reports drift; verification pack `30-verification/cargo-fmt.txt`.
  why: |
    Formatting noise mixed into substantive diffs makes review harder for the
    next pass. One cleanup commit isolates the noise.
  recommendation: |
    Single commit `chore: cargo fmt across wm-* files` before wm-13. Add a
    pre-commit hook in CLAUDE.md tooling notes if it's recurring.
  confidence: high
```

## Synthesis

Three things matter most, in this order:

**1. Land the correctness fix on the search path before anything else.** `search-min-similarity-dropped-on-hybrid` is a quiet quality regression that gets harder to spot once gold-set scoring starts blaming embedding choices for what's actually a missing filter. Fix this first. While the hybrid path is intentional scaffolding (sparse leg not fully wired), the SearchArgs contract is already public — the fix is a one-field plumb plus a test that pins the floor. This is the only finding that I'd want resolved before more wm-* work piles on top.

**2. Consolidate the validators split now, while it's one function vs eleven.** This is design debt, not a bug, but it's debt that compounds linearly with every new contract. The path of least resistance for the next wm-* author is to add their validator to whichever file matches the call site they're in — and that ratchets the wrong way. Same commit can take care of `supersede-soft-delete-interaction` (one explicit precondition), `clippy-supersede-unwrap`, and the rename of `exclude_retired` → `exclude_superseded`. That's a tight "shape and naming" pass.

**3. Slop cleanup as one commit.** Format drift, possibly-dead ingest defaults, the `reflect_summary` rename, the `query_log` sparse-column decision, the get_profile tiebreaker. Everything in this bucket is small, and shipping them as one "cleanup before wm-13" commit keeps subsequent wm-* diffs readable. This is exactly the kind of work that gets deferred into v0.1 release prep and is then ten times more expensive.

**Root causes vs symptoms.** Two of the high/medium findings (validators-split-incomplete, supersede-soft-delete-interaction) trace to the same root: the pivot introduced new state-space expansions (memory_type taxonomy, soft-delete + supersede + invalidate) faster than the constraint surface caught up. The fix isn't more constraints — it's pulling the contracts that already exist into one place where someone editing them can see them all. The min_similarity drop is a symptom of the hybrid leg being scaffolded incrementally without a contract test pinning what the public API guarantees regardless of which leg runs. A single `tests/contract.rs` test that asserts "min_similarity floor holds across all retrieval modes" would have caught it.

The pivot itself is on a healthy trajectory. None of these findings change the verdict — keep going. Adjustments above, then wm-13 onward.

## Slop list

**Pivot-introduced (wm-1..wm-12)**

1. `src/tools/search.rs:62` — `CONSOLIDATED_TYPES` const flagged as possibly dead by sutra; verify it's still consumed by the default-types branch (it is, line 192) — false positive, drop from the dead list.
2. `src/tools/search.rs:31` — `SNIPPET_CHARS` flagged dead; consumed at line 286 — false positive.
3. `src/tools/supersede.rs:85` — `unwrap` after `is_some` (clippy).
4. `src/tools/validate.rs:304` — collapsible-if (clippy).
5. `src/tools/reflect_summary.rs` — name doesn't match behaviour; rename to `reflect_status`.
6. `src/validators.rs:7` — `DECISION_ERROR_MSG` flagged dead; consumed inside the same file's tests and the validator — verify it's still referenced by `validate_decision_metadata`. If yes, false positive; if it was inlined, delete the const.
7. `src/tools/search.rs` — `truncated` flag overloaded (see finding).
8. `src/tools/search.rs:94` — `exclude_retired` → `exclude_superseded` rename.
9. Format drift across `chitta-migrate`, `scoring`, `validators`, `tools/*`, `tests` (verification pack).
10. `tests/contract.rs:11` — unused import `AppliesTo` (clippy).
11. `src/tools/get_profile.rs:76` — non-deterministic sort on ties.

**Pre-pivot (carry-over, not introduced by wm-*)**

12. `src/main.rs:24` — `Cli` struct flagged dead; verify clap derive isn't being macro-consumed elsewhere. If genuinely dead, delete.
13. `src/main.rs:519` — `shutdown_signal` flagged dead; confirm wire-up in axum router. If unused, delete or wire it before HTTP transport lands.
14. `src/ingest.rs:35,38` — `default_profile`, `default_source` likely orphaned by serde-default removal.
15. `src/ingest.rs:109` — `NARRATION_PREFIXES` const possibly orphaned; verify.
16. `src/embedding.rs:54,55` — `SPARSE_MISSING_WARN`, `SPARSE_EXTRACT_WARN` — likely log-once flags; verify before deletion.
17. `src/embedding.rs:387,403` — `Embedder::acquire_session`, `Embedder::replace_session` — pool methods possibly superseded by simpler API. Confirm against current `embed_full` callers.
18. `src/main.rs:470` — `tower_http::auth::...::bearer` deprecation warning. Resolve when HTTP transport lands; not blocking.
19. `src/mcp.rs:227` — `ChittaServer::get_info` — rmcp-trait call site; expected, drop from dead list.

Items 1, 2, 6, 19 are sutra false-positives — flag them in the SUMMARY's "likely-real dead code" caveat list rather than chasing them.
