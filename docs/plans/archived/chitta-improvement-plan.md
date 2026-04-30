# chitta improvement plan

Status: planned
Date: 2026-04-30
Source: `docs/chitta-overall-refactor.md` (findings, six candidates)
Related: `docs/plans/aion-share-refactor.md` (engine/server split)

Implementation plan for the six candidates in `chitta-overall-refactor.md`.
The candidates target depth, locality, and leverage. Several of them are
also prerequisites for the aion-share crate split, so this plan is
explicitly framed as the cleanup-before-split phase: each step makes the
eventual split mechanical rather than design-under-pressure.

## ordering rationale

The refactor doc's sequencing notes pair candidate 1 (newtypes) with
candidate 4 (config to validate dependency inversion) because the
`MemoryType` newtype naturally absorbs `VALID_MEMORY_TYPES`. Candidate 5
(curate `lib.rs`) is the foundational seam for the engine/server split.
Candidates 2 (retrieval consolidation) and 3 (extract admin) are
independent and can land in any order.

Phase ordering chosen:

1. **Phase 1 — Extract admin** (candidate 3). Lowest risk. Pure move.
   Establishes the integration-test seam for `replay`/`backfill`.
2. **Phase 2 — Retrieval consolidation** (candidate 2). Highest leverage
   for retrieval-quality work. Independent of crate boundaries.
3. **Phase 3 — Domain newtypes + config inversion** (candidates 1 + 4).
   Done together; the `MemoryType` newtype absorbs the allowlist and
   becomes the implementation of aion-share work item B.
4. **Phase 4 — Curate `lib.rs`** (candidate 5). Done as the final step
   before the crate split, so the public surface is curated against the
   actual depth of every other module.
5. **Phase 5 — Consolidate thin handlers** (candidate 6). Deferred. See
   "open question 1" below — this likely lands as part of the engine
   crate's `ops/` reorganisation rather than separately.

Phases 1-4 are net positive whether or not the aion-share split lands.
Phases 1 and 2 should commit independently; phase 3 is one logical unit.

## phase 1 — extract admin commands

**Goal:** `run_replay` and `run_backfill` become library functions.

**Files touched:** `src/main.rs`, new `src/admin.rs`, `src/lib.rs`.

**Steps:**

1. New module `src/admin.rs` with two `pub` async fns:
   - `pub async fn replay(pool: &PgPool, profile: Option<&str>, limit: i64) -> Result<ReplaySummary>`
   - `pub async fn backfill(pool: &PgPool, embedder: Arc<Embedder>, batch_size: i64) -> Result<BackfillSummary>`
2. Move the bodies from `main.rs` (lines 156-325). Drop the
   `Config::from_env` / `db::connect` / `tracing_subscriber::fmt` setup —
   that stays in `main.rs`. The library functions take pool + embedder
   parameters and return summary structs.
3. Move the table-printing UI for `replay` to `main.rs` — the library
   returns `ReplaySummary { entries: Vec<ReplayEntry>, avg_overlap: f64 }`,
   the binary formats it. Same for `backfill`: library returns
   `BackfillSummary { rows_updated: u64 }`, binary prints.
4. `main.rs` keeps `run_replay` / `run_backfill` as ~10-line orchestrators:
   build pool/embedder, call `admin::*`, print result.
5. Add `pub mod admin;` to `lib.rs` (will be curated in phase 4).
6. Add an integration test `tests/admin_backfill.rs` that calls
   `admin::backfill` against a test pool with a deliberately
   sparse-embedding-null row, asserts the row gets updated. Equivalent
   smoke test for replay against a seeded `query_log`.

**Acceptance:**

- `cargo build && cargo test` green.
- `chitta replay` and `chitta backfill` CLI behaviour unchanged
  (verified by running them against a dev DB).
- New integration tests pass.

**Commit:** one commit. ~250 LOC moved, ~80 LOC of new test.

## phase 2 — consolidate retrieval

**Goal:** retrieval scoring/ranking/dedup/budget logic lives in one
module with a small public interface. `db.rs` returns rows; `tools/search.rs`
becomes a thin dispatcher.

**Files touched:** `src/retrieval.rs`, `src/tools/search.rs`, `src/db.rs`.

**Steps:**

1. **Define the deep interface.** In `retrieval.rs`:
   ```
   pub struct RetrievalRequest<'a> {
       pub profile: &'a str,
       pub query_text: &'a str,
       pub query_embed: &'a EmbedOutput,
       pub k: i64,
       pub tags: &'a [String],
       pub memory_types: &'a [String],
       pub min_similarity: f32,
       pub max_tokens: Option<u64>,
       pub include_content: bool,
   }
   pub struct RetrievalResponse {
       pub hits: Vec<RetrievedHit>,
       pub total_available: i64,
       pub truncated: bool,
   }
   pub async fn search(pool: &PgPool, cfg: &SearchConfig, req: RetrievalRequest<'_>) -> Result<RetrievalResponse>;
   ```
   `RetrievedHit` is the unified retrieval-layer hit (replaces the
   `db::SearchHit` + `tools::search::SearchHit` duplication — see open
   question 2).

2. **Move scoring out of `db.rs`.** Delete the recency math at
   `db.rs:426-437`. `db::search_by_embedding` returns rows ordered by raw
   cosine similarity (no recency boost). Recency stays in `retrieval.rs`
   (it already lives there for the hybrid path).

3. **Fix the `min_similarity` drop.** The hybrid path currently passes
   `0.0` to the dense leg (`retrieval.rs:37`). After consolidation,
   `min_similarity` filtering happens once in `retrieval::search`, after
   fusion, against the dense similarity. Document the semantics: it is a
   cosine-similarity floor on the dense leg, not on the fused score.

4. **Move `dedup_by_field` and `apply_budget` from `tools/search.rs`
   into `retrieval.rs`.** Their tests move with them.

5. **Make `apply_type_weights` internal.** It becomes `pub(crate)` (or
   private) and is called inside `search` for both hybrid and non-hybrid
   paths — eliminating the "caller must know which path applied weights"
   leak.

6. **Slim `tools/search.rs`.** It becomes: validate args, embed,
   build `RetrievalRequest`, call `retrieval::search`, map
   `RetrievedHit` to wire-shape `SearchHit` (snippet + optional content),
   build envelope, fire query log. Target: ~80 lines.

7. **Tests.** Unit tests for `dedup_by_field`, `apply_budget`, recency
   math, RRF formula remain. Add an integration-style test that drives
   the full `retrieval::search` pipeline (dedup + budget + recency
   together) without going through MCP — currently impossible because
   those algorithms aren't reachable.

**Acceptance:**

- `cargo test` green, including new pipeline test.
- Existing search integration tests unchanged (behaviour preserved).
- Retrieval-related grep over `tools/search.rs` shows no scoring math —
  only validate, embed, dispatch, envelope.

**Commit:** one commit, possibly two if the `SearchHit` rename lands
separately.

## phase 3 — domain newtypes + config inversion

**Goal:** `validate.rs` shrinks to type definitions. `MemoryType` is
constructed from the config-driven allowlist. The `tools::validate ←
config::` import direction is reversed.

**Files touched:** new `src/types.rs`, `src/config.rs`,
`src/tools/validate.rs` (becomes thin or vanishes), every tool handler.

**Steps:**

1. **Create `src/types.rs`** with newtypes for the inputs that have real
   constraints:
   - `Profile(String)` — 1-128 chars, `[a-zA-Z0-9_-]+`
   - `Tags(Vec<String>)` — up to 32 entries, each 1-64 chars
   - `IdempotencyKey(String)` — 1-128 chars, no control chars
   - `MemoryType(String)` — validates against an allowlist (see step 2)
   - `EventTime(DateTime<Utc>)` — epoch <= t <= now+365d
   - `K(i64)` — 1..=200
   - `MinSimilarity(f32)` — finite, 0.0..=1.0
   - `MaxTokens(u64)` — > 0
   Each has `TryFrom<&str>` (or the appropriate input) returning
   `ChittaError::InvalidArgument`. The error carries `tool: &'static str`
   set by the caller via a builder method (`Profile::try_from_for(tool, value)`)
   or via a thin per-tool wrapper. See open question 3 for the call-site
   shape.

2. **Add `Config::allowed_memory_types: Vec<String>`** populated from
   `CHITTA_MEMORY_TYPES` (comma-separated env var), default = the current
   five (`memory|observation|decision|session_summary|mental_model`).
   This is the implementation of aion-share work item B; checking it off
   here closes that item.
   - Validate at config load: any `type_weights` key not in the
     allowlist is a config error (per aion-share open question on
     "type-weights config interplay").
   - `MemoryType::try_from_for(tool, value, &allowlist)` — the registry
     is passed in, not stored statically. Construction sites that need
     it: `tools::store::handle`, `tools::update::handle`,
     `tools::search::handle`, `tools::list::handle`. They already have
     `&Config` or `&SearchConfig` in scope; thread the allowlist through.

3. **Delete the `tools::validate ← VALID_MEMORY_TYPES` import** in
   `config.rs:144`. Config now owns its own vocabulary.

4. **Migrate handlers.** Each handler:
   - destructures args to raw types,
   - constructs the newtypes (returning `InvalidArgument` on failure),
   - passes the validated values to `db::*`. The DB layer keeps taking
     `&str` / `&[String]` — newtypes are a handler-boundary concern, not
     a DB-layer concern (see open question 4).

5. **Shrink or delete `tools/validate.rs`.** What remains:
   - `parse_uuid` (moves to `types.rs` or `error.rs` as a free fn)
   - `MAX_CONTENT_BYTES`, `MAX_K` constants (move to `types.rs`)
   - the byte-length and non-empty checks for `content` and `query`
     (these are not domain types — keep as free functions in `types.rs`
     or inline at handlers; they have no ambiguity)
   - the validate.rs unit tests — they migrate to `types.rs` as
     `TryFrom` tests.

6. **Documentation.** Add the env var to the official version docs (per
   aion-share work item B step 5). Update `docs/principles.md` with the
   per-deployment vocabulary policy if not already done (aion-share
   work item B step 1).

**Acceptance:**

- `cargo build && cargo test` green.
- `CHITTA_MEMORY_TYPES=foo,bar` server accepts `memory_type: "foo"` and
  rejects `memory_type: "memory"`. No env var → five legacy types
  accepted (matches aion-share acceptance criterion).
- `rg 'tools::validate' src/` returns no hits in `config.rs`.
- `validate.rs` is gone or under ~40 lines.

**Commit:** one or two commits — newtypes + handler migration is one
unit; the allowlist config plumb-through can be split if it gets large.

## phase 4 — curate lib.rs

**Goal:** `lib.rs` exposes a curated public API. Internal plumbing is
`pub(crate)`. The crate boundary is a real seam.

**Files touched:** `src/lib.rs`, every module that has overly-public
symbols.

**Steps:**

1. **Inventory the public surface.** Walk every `pub` item in `db.rs`,
   `embedding.rs`, `envelope.rs`, `error.rs`, `retrieval.rs`,
   `tools/*`, `config.rs`. For each, decide:
   - **External (`pub`)**: needed by integration tests, `main.rs`, or
     future engine consumers (per aion-share, this is the engine's
     export set).
   - **Crate-internal (`pub(crate)`)**: used across modules but not by
     consumers.
   - **Module-private**: used only within its own module.

2. **Likely demotions to `pub(crate)`** based on the refactor doc's
   examples:
   - `db::is_unique_violation`
   - `db::fetch_sparse_embeddings`
   - `db::PG_UNIQUE_VIOLATION`
   - `retrieval::apply_type_weights` (after phase 2 pulls it inside
     `retrieval::search`)
   - `embedding::*` internal helpers

3. **Likely public surface (engine exports)** — what `chitta-engine`
   will expose after the aion-share split:
   - `Config` and its loader
   - `db::connect`, `db::run_migrations`, `db::store_memory`,
     `db::get_memory`, `db::update_memory`, `db::delete_memory`,
     `db::list_memories` (or whatever the engine `ops/` shape becomes)
   - `Embedder` (until phase C of aion-share extracts it to a sidecar)
   - `retrieval::search`, `RetrievalRequest`, `RetrievalResponse`
   - `Envelope`, `estimate_tokens`
   - `ChittaError`, `Result`
   - `admin::replay`, `admin::backfill`
   - newtypes from phase 3

4. **Replace flat `pub mod`s with selective `pub use`** in `lib.rs`:
   ```
   mod admin;
   mod config;
   mod db;
   mod embedding;
   mod envelope;
   mod error;
   mod retrieval;
   mod types;

   pub use admin::{backfill, replay, BackfillSummary, ReplaySummary};
   pub use config::Config;
   pub use db::{connect, run_migrations, /* …chosen ops */};
   pub use embedding::{Embedder, EmbedOutput};
   pub use envelope::{estimate_tokens, Envelope};
   pub use error::{ChittaError, Result};
   pub use retrieval::{search, RetrievalRequest, RetrievalResponse, RetrievedHit};
   pub use types::{IdempotencyKey, K, MaxTokens, MemoryType, MinSimilarity, Profile, Tags};

   pub mod mcp;   // server-side; will move to chitta-server in the split
   pub mod tools; // server-side; will move with mcp
   ```
   `mcp` and `tools` stay `pub mod` because they are the bridge layer
   that will move out wholesale during the crate split — curating their
   surface internally is wasted work.

5. **Fix anything that breaks.** Integration tests under `tests/`
   should be the only external consumer; they may need import path
   adjustments, which is exactly the signal that the boundary is now
   meaningful.

**Acceptance:**

- `cargo build && cargo test` green.
- `lib.rs` lists exports, not modules.
- `cargo check` on a synthetic external consumer (a tiny scratch
  binary that imports `chitta::Config` etc.) compiles using only the
  curated surface.

**Commit:** one commit. Mostly visibility changes, no behaviour change.

## phase 5 (deferred) — consolidate thin tool handlers

The refactor doc proposes merging `tools/{delete,get,list,health}.rs`
into a `tools/crud.rs`. This is real waste today, but the aion-share
plan moves these into `chitta-engine/src/ops/*` during the crate split
— at which point reorganisation happens anyway. Doing it twice is
wasteful.

**Decision:** skip as a standalone phase. Roll into the aion-share
engine split: when the ops move to `chitta-engine`, organise them as
`ops/crud.rs` (get, delete, list) + `ops/health.rs` + `ops/store.rs` +
`ops/update.rs` + `ops/search.rs`. This is a pure reorganisation
disguised as a move and costs nothing extra.

If aion-share is delayed indefinitely, revisit and run phase 5
standalone.

## additional observations from the refactor doc

The "additional observations" section of `chitta-overall-refactor.md`
lists six smaller items. None block the candidates above; treat them as
follow-ups.

| observation | proposed disposition |
|---|---|
| `error.rs` mixes startup + runtime errors | defer; touch when building the engine crate (split startup errors into a `StartupError` exposed only to `main`) |
| `embedding::embed_full` is 200 lines, 5 phases inside `spawn_blocking` | defer to aion-share phase C (embedder sidecar) — the sidecar protocol forces the seam |
| `db::update_memory` has a 10-parameter signature | quick win: introduce `UpdateMemoryRequest` struct as part of phase 3 or as a 1-commit follow-up |
| `mcp.rs` 7 copy-pasted `to_string_pretty` closes | trivial cleanup; do anytime, not part of any phase |
| `SearchHit` defined in two places with different shapes | resolved by phase 2 — rename retrieval-layer to `RetrievedHit` |
| `MAX_TOKENS` in `embedding.rs` vs hardcoded "8192" in `error.rs` drift risk | trivial: import the constant in `error.rs` — do as part of phase 3 |

## risks

- **Behaviour drift in retrieval (phase 2).** Moving recency out of
  `db.rs` into `retrieval.rs` for the non-hybrid path is a real change:
  today the non-hybrid SQL applies recency in the ORDER BY; after, it
  applies post-fetch in Rust. Numerically equivalent for a single page
  of results but means the SQL `ORDER BY` is now by raw similarity.
  Need to verify the integration test snapshots stay valid.
- **`MemoryType` allowlist breaking benchmarks (phase 3).** If
  benchmark fixtures use a memory type not in the default allowlist,
  they break unless `CHITTA_MEMORY_TYPES` is set. Audit fixtures before
  shipping the allowlist enforcement.
- **`pub(crate)` demotion churn (phase 4).** Surfacing the boundary may
  reveal that something we thought was internal is actually used by an
  integration test. Each such case is a useful signal but adds rework.

## design decisions you need to make

The following decisions cannot be made unilaterally — they shape phases
2, 3, and 4 in directly visible ways. My recommendation follows each.

### 1. Should phase 5 (consolidate thin handlers) run standalone, or wait for the aion-share engine split?

**Why it matters:** doing it now means a one-commit cleanup but creates
a reorganisation that the engine split will redo. Doing it during the
split means the cleanup is free, but only happens when aion-share
actually lands — which is not yet scheduled.

**My recommendation:** wait. Aion-share's work item A is well-specified
and the engine split is on the near-term roadmap. If three months pass
without aion-share moving, run phase 5 then.

### 2. In phase 2, unify `db::SearchHit` and `tools::search::SearchHit` into a single retrieval-layer `RetrievedHit`?

**Why it matters:** today both structs exist and the wire shape happens
to overlap with the DB shape, which is fragile. `tools::search` then
maps DB-shape into wire-shape (snippet, optional content). Unifying
into `RetrievedHit` (the retrieval module's owned type) clarifies
ownership but is one more rename in the diff.

**My recommendation:** unify. The doc explicitly calls out this
duplication and the layering is correct (retrieval owns the type, DB
populates it, tools maps to wire). Land it as a separate commit at the
end of phase 2 to keep the diff readable.

### 3. In phase 3, how should newtypes carry the `tool` discriminator for error messages?

**Options:**

a. **Builder method per tool**: `Profile::try_from_for(tool: &'static str, value: &str)`. Pros: error messages keep their per-tool context. Cons: every call site repeats `Profile::try_from_for(TOOL, &args.profile)?` — verbose.

b. **Drop the per-tool context.** Errors carry `argument` only, not `tool`. Pros: clean `TryFrom`. Cons: error messages get less specific (the existing tests assert on tool names).

c. **Per-tool wrapper module.** A small `tools::store::validate` module wraps newtype construction with the tool name baked in. Pros: handler call sites stay clean (`validate::profile(&args.profile)?`). Cons: re-introduces the validate-module shape we're trying to eliminate, just per-tool.

**My recommendation:** option (a). It's the most honest about what's
happening: newtype construction is contextual, and the context is the
tool name. The verbosity at call sites is acceptable — each tool
handler has at most ~5 newtype constructions, and a future
`#[tool(name="store_memory")]`-style derive can shrink it later. Option
(b) is a regression in error quality. Option (c) reproduces the
original problem.

### 4. Do newtypes propagate to `db.rs`, or stop at the handler boundary?

**Why it matters:** if `db::store_memory` takes `&Profile` instead of
`&str`, the type guarantees flow all the way down — but `db.rs` is
intended to be pure SQL with no domain knowledge. If newtypes stop at
the handler, `db.rs` keeps its current `&str` signatures and the
newtype is destructured at the call site (`db::store_memory(profile.as_str(), ...)`).

**My recommendation:** stop at the handler boundary. `db.rs` is a
fetch/persist layer; it should not know about domain types. Construction
of the newtype already proved validity — passing `&str` downstream is
fine. Bonus: this is what the aion-share engine split implies (the
engine's `db.rs` and `ops/*` should remain decoupled).

### 5. Should phase 4 (curate `lib.rs`) happen before or as part of the aion-share crate split?

**Why it matters:** the refactor doc says phase 4 is the foundational
seam for the split. If it lands before, the split becomes a `Cargo.toml`
change. If it lands as part of the split, you do both decisions under
the same diff — which is more pressure but avoids two visibility passes.

**My recommendation:** before. The refactor doc explicitly recommends
this, and curating the surface is forced clarity that pays off
regardless of whether the split happens. The split itself becomes
pure-mechanical.

### 6. Run phase 1-4 sequentially with separate PRs/commits, or as one larger refactor?

**Why it matters:** sequential commits are easier to review and bisect.
A larger refactor lets the four phases inform each other (e.g. phase 4
might want to revisit a phase 2 export decision).

**My recommendation:** sequential commits, each on its own. The four
phases are genuinely independent. If phase 4 wants to revisit a phase 2
decision, that's a follow-up commit, not a reason to bundle.

## acceptance for the whole plan

- All four phases (1-4) committed.
- `validate.rs` deleted or under 40 lines.
- `lib.rs` exports a curated API, not a module list.
- No retrieval scoring math outside `retrieval.rs`.
- Admin commands callable from integration tests.
- `CHITTA_MEMORY_TYPES` env var honoured.
- Aion-share work item B closed; work item A (engine split) reduced to
  a Cargo workspace + module move with no design surface left to
  resolve.
