# chitta refactor

Status: planned
Date: 2026-04-30 (renamed and expanded from aion-share-refactor.md, originally 2026-04-26)
Source for cleanup phases: `docs/archived/chitta-overall-refactor.md`

Comprehensive refactor plan for chitta-rs. Three goals, sequenced:

1. **Phase 0 — cleanup.** Four sub-phases addressing the six candidates
   in `chitta-overall-refactor.md` (depth, locality, leverage).
2. **Phase 1 — engine + server crate split.** So chitta can be shared
   between manas (developer cognition os) and aion (astrologer's os,
   public ship target).
3. **Phase 2 — embedder sidecar.** So multiple chitta consumers on the
   same machine don't each load a 1.6GB model.
4. **Phase 3 — aion install engineering.** Lives in the aion repo,
   tracked there.

Phase 0 is framed as cleanup-before-split: each sub-phase removes a
design pressure that would otherwise land mid-split. With phase 0
done, phase 1 becomes a `Cargo.toml` change plus mechanical moves.

## context

Chitta was originally designed for manas — a single consumer running
under claude code as the dev's cognitive memory. Aion has now been
designed and contains a memory subsystem of its own: research notes,
voice-dictated session transcripts, citations from books and papers,
chart-linked observations. The shape is the same as chitta — verbatim,
bi-temporal, profile-isolated, embedded similarity search, idempotent
writes, tags + metadata.

Re-implementing this for aion would be rebuilding chitta. So aion uses
chitta. But aion's deployment differs from manas's deployment in
non-trivial ways, and chitta today has a few hardcoded shapes that
don't survive a second consumer.

This doc captures the prep work in chitta itself. Aion-side work
(plugin manifest, ui, voice capture pipeline) lives in the aion repo
and is tracked there.

## decisions

### shared via engine + server split

Two crates in a Cargo workspace:

- **chitta-engine** — storage, embedder, retrieval, validation,
  envelope, error, config, admin. No MCP, no transport, no I/O surface
  besides the database.
- **chitta-server** — thin MCP wrapper over the engine. rmcp tool
  router, stdio + streamable-http transports, main.rs binary.

One implementation. Two deployments:

| consumer | how chitta runs | db location |
|---|---|---|
| manas | `chitta serve` (stdio), claude code is the client | dev's postgres, dedicated database |
| aion | bundled mcp plugin (stdio child of aion's plugin host) | user's postgres, dedicated database |

Two separate processes against two separate databases. They share the
binary and the codebase.

### postgres stays — install as a real dep

Considered three storage paths:

| option | rejected because |
|---|---|
| abstract `Repo` trait (postgres + sqlite backends) | maintaining two backends, two migration sets, two test matrices forever — worse than the install-engineering cost |
| embedded postgres (pg_embed-style, managed by aion) | brittle on major-version upgrades; data-dir reinit pain; awkward process supervision per-os |
| **postgres as a real install dep** | aion's installer is going to be intense regardless (drishti + swiss ephemeris C lib + bge-m3 + whisper.cpp + geonames sqlite). adding postgres to the prereq list is one-time install engineering, not ongoing maintenance |

Cross-platform install reality:

- linux: easy (`apt install postgresql`, setup script for db + role)
- macos: workable (postgres.app or brew). aion can detect either
- windows: hardest (edb installer or scoop). most astrologers are on
  windows today, so this matters and will be the painful piece. accepted
  as a one-time engineering cost rather than an ongoing two-backend tax

Benefit: same binary runs against the same backend in both deployments.
v0.0.3's hybrid-retrieval work (tsvector + GIN + JSONB) keeps working
unchanged. Single benchmark surface. Users running both manas and aion
on the same machine can share a postgres instance via separate databases.

### memory_type → deployment-configured allowlist

Current state: the DB column is plain `text` (migration 0005); the tool
layer hard-codes `VALID_MEMORY_TYPES = ["memory", "observation",
"decision", "session_summary", "mental_model"]` in
`src/tools/validate.rs:192` and rejects anything else.

New state: the allowlist is read from config (env var or config file),
default = the current five. Each deployment sets its own vocabulary:

- manas: `memory|observation|decision|session_summary|mental_model|document_ref`
- aion: `research_note|client_session|citation|transit_observation|dream|...`
  (final list to be settled in aion)

Allowlist (rather than open text) is kept because:
- catches typos in client code at the boundary
- keeps consistency within a deployment
- documents intent without forcing a schema migration

This is a deliberate revision relative to the current principle 5
("small core, grow by evidence") — it loosens the contract without
opening it. A short principle-doc revision PR should land before the
behavior change. **Implementation is folded into phase 0.3** (newtypes
+ allowlist) — see below.

### embedder extracted as a sidecar service

When aion lands, two chitta processes (manas + aion) and aion's chart-db
plugin all need the same BGE-M3 model. Three processes each loading
1.6GB is unviable on the 8-16GB laptop target.

Extract the embedder into a small sidecar:

- separate process, autostart with the host
- tiny protocol over unix socket: `{text, mode: dense|hybrid}` →
  `{dense: [f32; 1024], sparse: {token: weight}}`
- idle timeout to unload model and free RAM (per the manas memory)
- chitta-engine speaks to the sidecar via the embedder client trait it
  already has internally; the in-process impl stays available for
  test/single-process deployments

Repo location: TBD. Could live in chitta's repo as a new crate, or be
its own repo (candidate name: `pratyaksha` — "direct perception").
Defaulting to chitta's repo unless there's a reason to split.

## sequencing

```
Phase 0  cleanup (0.1 → 0.4, mostly independent)   ── ~3-4 days
   ↓
Phase 1  engine + server crate split               ── ~1-2 days, mechanical
   ↓
   v0.0.3 lands on postgres (hybrid retrieval) — unchanged
   ↓
Phase 2  embedder service extraction               ── ~1 week
   ↓
Phase 3  aion install engineering                  ── in aion repo
```

Phases 0 and 1 are pure prep — they can land now without disturbing
v0.0.3. Phase 2 blocks aion's first usable build because aion + manas
can't both hold the model. Phase 3 depends on 0, 1, and 2 all stable.

Inside phase 0, the four sub-phases are independent and commit
separately. Recommended order: 0.1 → 0.2 → 0.3 → 0.4 (lowest risk first,
public surface curated last so it reflects the actual depth of the
other modules).

---

## phase 0 — cleanup before split

Six candidates from `chitta-overall-refactor.md`, sequenced into four
sub-phases. Candidate 6 (consolidate thin tool handlers) is deferred
and folded into phase 1: when the ops move to `chitta-engine`, organise
them as `ops/crud.rs` (get, delete, list) + `ops/health.rs` + the three
substantive ops. This is a pure reorganisation that costs nothing extra
during the split, so running it standalone first would just be
redoing work.

### 0.1 — extract admin commands

**Candidate:** 3.

**Goal:** `run_replay` and `run_backfill` become library functions,
testable from integration tests.

**Files touched:** `src/main.rs`, new `src/admin.rs`, `src/lib.rs`.

**Steps:**

1. New module `src/admin.rs` with two `pub` async fns:
   - `pub async fn replay(pool: &PgPool, profile: Option<&str>, limit: i64) -> Result<ReplaySummary>`
   - `pub async fn backfill(pool: &PgPool, embedder: Arc<Embedder>, batch_size: i64) -> Result<BackfillSummary>`
2. Move the bodies from `main.rs` (lines 156-325). Drop the
   `Config::from_env` / `db::connect` / `tracing_subscriber::fmt` setup
   — that stays in `main.rs`. The library functions take pool + embedder
   parameters and return summary structs.
3. Move the table-printing UI for `replay` to `main.rs`: library returns
   `ReplaySummary { entries: Vec<ReplayEntry>, avg_overlap: f64 }`,
   binary formats it. Same for `backfill`: library returns
   `BackfillSummary { rows_updated: u64 }`, binary prints.
4. `main.rs` keeps `run_replay` / `run_backfill` as ~10-line
   orchestrators: build pool/embedder, call `admin::*`, print result.
5. Add `pub mod admin;` to `lib.rs` (will be curated in 0.4).
6. Add an integration test `tests/admin_backfill.rs` that calls
   `admin::backfill` against a test pool with a deliberately
   sparse-embedding-null row, asserts the row gets updated. Equivalent
   smoke test for replay against a seeded `query_log`.

**Acceptance:** `cargo build && cargo test` green; CLI behaviour
unchanged on a dev DB; new integration tests pass.

**Commit:** one. ~250 LOC moved, ~80 LOC of new test.

### 0.2 — consolidate retrieval

**Candidate:** 2.

**Goal:** retrieval scoring/ranking/dedup/budget logic lives in one
module with a small public interface. `db.rs` returns rows; `tools/search.rs`
becomes a thin dispatcher.

**Files touched:** `src/retrieval.rs`, `src/tools/search.rs`, `src/db.rs`.

**Steps:**

1. **Define the deep interface** in `retrieval.rs`:
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
   `db::SearchHit` + `tools::search::SearchHit` duplication — see
   open question 1).

2. **Move scoring out of `db.rs`.** Delete the recency math at
   `db.rs:426-437`. `db::search_by_embedding` returns rows ordered by
   raw cosine similarity. Recency stays in `retrieval.rs`.

3. **Fix the `min_similarity` drop.** The hybrid path currently passes
   `0.0` to the dense leg (`retrieval.rs:37`). After consolidation,
   `min_similarity` filtering happens once in `retrieval::search`,
   after fusion, against the dense similarity. Document the semantics:
   it is a cosine-similarity floor on the dense leg, not on the fused score.

4. **Move `dedup_by_field` and `apply_budget` from `tools/search.rs`
   into `retrieval.rs`.** Their tests move with them.

5. **Make `apply_type_weights` internal.** It becomes `pub(crate)` (or
   private) and is called inside `search` for both hybrid and non-hybrid
   paths — eliminating the "caller must know which path applied weights"
   leak.

6. **Slim `tools/search.rs`.** Validate args, embed, build
   `RetrievalRequest`, call `retrieval::search`, map `RetrievedHit` to
   wire-shape `SearchHit` (snippet + optional content), build envelope,
   fire query log. Target: ~80 lines.

7. **Tests.** Unit tests for `dedup_by_field`, `apply_budget`, recency
   math, RRF formula remain. Add an integration-style test that drives
   the full `retrieval::search` pipeline (dedup + budget + recency
   together) without going through MCP.

**Acceptance:** `cargo test` green incl. new pipeline test; existing
search integration tests unchanged; `tools/search.rs` has no scoring math.

**Commit:** one or two (the `SearchHit` rename can land separately).

### 0.3 — domain newtypes + memory_type allowlist

**Candidates:** 1 + 4. **Closes original work item B** from the
aion-share plan (memory_type allowlist).

**Goal:** `validate.rs` shrinks to type definitions or vanishes.
`MemoryType` is constructed from the config-driven allowlist. The
`tools::validate ← config::` import direction is reversed.

**Files touched:** new `src/types.rs`, `src/config.rs`,
`src/tools/validate.rs` (becomes thin or vanishes), every tool handler.

**Steps:**

1. Land a principle-revision PR updating `docs/principles.md` to
   reflect the per-deployment vocabulary policy. (Originally aion-share
   work item B step 1.)

2. **Create `src/types.rs`** with newtypes for inputs that have real
   constraints:
   - `Profile(String)` — 1-128 chars, `[a-zA-Z0-9_-]+`
   - `Tags(Vec<String>)` — up to 32 entries, each 1-64 chars
   - `IdempotencyKey(String)` — 1-128 chars, no control chars
   - `MemoryType(String)` — validates against an allowlist
   - `EventTime(DateTime<Utc>)` — epoch <= t <= now+365d
   - `K(i64)` — 1..=200
   - `MinSimilarity(f32)` — finite, 0.0..=1.0
   - `MaxTokens(u64)` — > 0

   Each has a fallible constructor returning `ChittaError::InvalidArgument`.
   See open question 2 for the call-site shape.

3. **Add `Config::allowed_memory_types: Vec<String>`** populated from
   `CHITTA_MEMORY_TYPES` (comma-separated env var), default = the
   current five.
   - Validate at config load: any `type_weights` key not in the
     allowlist is a config error (closes the aion-share open question
     on type-weights config interplay).
   - `MemoryType::try_new(tool, value, &allowlist)` — registry passed
     in, not stored statically. Construction sites that need it:
     `tools::store::handle`, `tools::update::handle`,
     `tools::search::handle`, `tools::list::handle`. They already have
     `&Config` or `&SearchConfig` in scope; thread the allowlist through.

4. **Delete the `tools::validate ← VALID_MEMORY_TYPES` import** in
   `config.rs:144`. Config now owns its own vocabulary.

5. **Migrate handlers.** Each handler:
   - destructures args to raw types,
   - constructs the newtypes (returning `InvalidArgument` on failure),
   - passes the validated values to `db::*`. The DB layer keeps taking
     `&str` / `&[String]` — newtypes are a handler-boundary concern,
     not a DB-layer concern (open question 3).

6. **Shrink or delete `tools/validate.rs`.** What remains:
   - `parse_uuid` (moves to `types.rs` or `error.rs` as a free fn)
   - `MAX_CONTENT_BYTES`, `MAX_K` constants (move to `types.rs`)
   - byte-length and non-empty checks for `content` and `query` (these
     are not domain types — keep as free functions or inline)
   - migrate the unit tests to `types.rs` as constructor tests.

7. **Documentation.** Add the env var to the official version docs.

**Acceptance:**

- `cargo build && cargo test` green.
- `CHITTA_MEMORY_TYPES=foo,bar` server accepts `memory_type: "foo"`
  and rejects `memory_type: "memory"`. No env var → five legacy types
  accepted (matches original aion-share acceptance criterion for B).
- `rg 'tools::validate' src/` returns no hits in `config.rs`.
- `validate.rs` is gone or under ~40 lines.

**Commit:** one or two — newtypes + handler migration is one unit; the
allowlist config plumb-through can split off if it grows.

### 0.4 — curate lib.rs

**Candidate:** 5.

**Goal:** `lib.rs` exposes a curated public API. Internal plumbing is
`pub(crate)`. The crate boundary is a real seam — and is the
foundational seam for phase 1.

**Files touched:** `src/lib.rs`, every module with overly-public symbols.

**Steps:**

1. **Inventory the public surface.** For every `pub` item in `db.rs`,
   `embedding.rs`, `envelope.rs`, `error.rs`, `retrieval.rs`,
   `tools/*`, `config.rs`, decide:
   - **External (`pub`)**: needed by integration tests, `main.rs`, or
     the future engine's exports.
   - **Crate-internal (`pub(crate)`)**: cross-module but not consumer-facing.
   - **Module-private**: used only within its own module.

2. **Likely demotions to `pub(crate)`:**
   - `db::is_unique_violation`
   - `db::fetch_sparse_embeddings`
   - `db::PG_UNIQUE_VIOLATION`
   - `retrieval::apply_type_weights` (after 0.2 pulls it inside
     `retrieval::search`)
   - internal `embedding::*` helpers

3. **Likely public surface (engine exports for phase 1):**
   - `Config` and its loader
   - `db::connect`, `db::run_migrations`, the storage ops
   - `Embedder` (until phase 2 extracts it)
   - `retrieval::search`, `RetrievalRequest`, `RetrievalResponse`,
     `RetrievedHit`
   - `Envelope`, `estimate_tokens`
   - `ChittaError`, `Result`
   - `admin::replay`, `admin::backfill`
   - newtypes from 0.3

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

   pub mod mcp;   // server-side; moves to chitta-server in phase 1
   pub mod tools; // server-side; moves with mcp
   ```
   `mcp` and `tools` stay `pub mod` because they are the bridge layer
   that moves out wholesale during phase 1 — curating their surface
   internally is wasted work.

5. **Fix anything that breaks.** Integration tests under `tests/`
   should be the only external consumer; import-path adjustments are
   the signal that the boundary is now meaningful.

**Acceptance:**

- `cargo build && cargo test` green.
- `lib.rs` lists exports, not modules.
- `cargo check` on a synthetic external consumer (a tiny scratch binary
  importing `chitta::Config` etc.) compiles using only the curated surface.

**Commit:** one. Mostly visibility changes, no behaviour change.

---

## phase 1 — engine + server crate split

With phase 0 done, this becomes mechanical.

1. Convert `chitta-rs` to a Cargo workspace.
2. New crate `chitta-engine`. Move:
   - `src/config.rs`, `src/db.rs`, `src/embedding.rs`, `src/envelope.rs`,
     `src/error.rs`, `src/retrieval.rs`, `src/types.rs`, `src/admin.rs`
     → `chitta-engine/src/*`.
   - `src/tools/{store,get,search,update,delete,list,health}.rs`
     → `chitta-engine/src/ops/*`. **Reorganisation here folds in
     candidate 6**: get/delete/list/health collapse into `ops/crud.rs`
     + `ops/health.rs`; store/update/search keep their own files.
     Strip rmcp `Args` schema types — those stay server-side. Engine
     functions take plain rust args, return plain rust results.
3. New crate `chitta-server`. Move:
   - `src/mcp.rs` → `chitta-server/src/mcp.rs` (the rmcp wrapper)
   - `src/main.rs` → `chitta-server/src/main.rs`
   - `*Args` schema types stay here, deserialise from rmcp tool inputs,
     call into engine, wrap engine output into rmcp responses.
4. `Cargo.toml` for the workspace; per-crate `Cargo.toml`s with minimal
   feature sets.
5. CI matrix: build engine alone (no rmcp); build server (engine +
   rmcp); run all integration tests.

**Acceptance:** `cargo build -p chitta-engine` succeeds with no rmcp
dependency in the engine's tree. All existing tests pass.

## phase 2 — embedder service extraction

1. New crate `chitta-embedder` (or `pratyaksha` if it gets its own
   repo). Hosts an embedder pool and a unix-socket server.
2. Wire protocol — minimal length-framed JSON or msgpack:
   - `Embed { input: Text(String) | Image(bytes), mode: "dense" | "hybrid" }`
   - response: `{ dense: [f32], sparse: Option<Map<String, f32>> }`
   - **Multimodal from day one.** The input enum accepts images even
     if the initial model (BGE-M3) only handles text. Grantha (document
     intelligence layer atop smriti) will send page images when local
     multimodal embedding models mature. Designing the interface now
     avoids retrofitting every consumer later. The server returns an
     error for unsupported modalities until a multimodal model is loaded.
3. Client crate (used by chitta-engine and eventually chart-db) that
   speaks the protocol; falls back to in-process embedder if the socket
   is unset (test/single-process deployments).
4. Idle eviction: unload model after N minutes idle, reload on next
   request. Tunable.
5. Service has its own binary + manifest entries for both manas and
   aion to spawn it.
6. Update chitta-engine to use the client by default when
   `CHITTA_EMBEDDER_SOCKET` is set; keep the in-process path for tests
   and small deployments.

**Acceptance:** two chitta-server processes pointing at the same
embedder socket can both `store_memory` concurrently, only one BGE-M3
model is resident in RAM, and idle eviction works.

## phase 3 — aion install engineering

Lives in the aion repo, not here. Tracked there. Touchpoints with chitta:

- chitta-server bundled as an aion plugin manifest entry (stdio,
  autoStart, db url from config)
- aion's installer runs postgres setup (detect, install if needed,
  create role, create `aion_chitta` db, run chitta migrations)
- aion ships the embedder sidecar as a separate manifest entry,
  exporting its socket path; chitta and chart-db both read it from env

---

## what this is NOT

- **Not a storage backend abstraction.** Postgres stays. SQLite is not
  added. The `Repo` trait idea is explicitly deferred — revisited only
  if windows install pain proves unacceptable in field testing.
- **Not a v0.0.3 reroute.** v0.0.3 (hybrid retrieval + agent-native
  quality) lands on the current single-crate codebase. Phase 0 can
  land in parallel; phase 1+ start after v0.0.3 ships, or on a parallel
  branch that rebases.
- **Not a public/multi-user pivot.** Profiles remain the only isolation
  primitive (principle 7). Each install is single-postgres-instance,
  one db per consumer, profiles within for client/topic scoping.
- **Not a chitta UI project.** Aion will build its notebook ui on top
  of chitta's MCP tools. Chitta itself stays headless.

## additional observations from chitta-overall-refactor.md

The "additional observations" section of the source doc lists six
smaller items. None block the phases above; treat them as follow-ups.

| observation | proposed disposition |
|---|---|
| `error.rs` mixes startup + runtime errors | defer; touch during phase 1 (split startup errors into `StartupError` exposed only to `main`) |
| `embedding::embed_full` is 200 lines, 5 phases inside `spawn_blocking` | defer to phase 2 — the sidecar protocol forces the seam |
| `db::update_memory` has a 10-parameter signature | quick win: introduce `UpdateMemoryRequest` struct as part of 0.3 or as a 1-commit follow-up |
| `mcp.rs` 7 copy-pasted `to_string_pretty` closes | trivial cleanup; do anytime, not part of any phase |
| `SearchHit` defined in two places with different shapes | resolved by 0.2 — rename retrieval-layer to `RetrievedHit` |
| `MAX_TOKENS` in `embedding.rs` vs hardcoded "8192" in `error.rs` drift risk | trivial: import the constant in `error.rs` — do as part of 0.3 |

## risks

- **Behaviour drift in retrieval (0.2).** Moving recency out of `db.rs`
  into `retrieval.rs` for the non-hybrid path is a real change: today
  the non-hybrid SQL applies recency in the `ORDER BY`; after, it
  applies post-fetch in Rust. Numerically equivalent for a single page
  of results but means the SQL `ORDER BY` is by raw similarity. Verify
  the integration test snapshots stay valid.
- **`MemoryType` allowlist breaking benchmarks (0.3).** If benchmark
  fixtures use a memory type not in the default allowlist, they break
  unless `CHITTA_MEMORY_TYPES` is set. Audit fixtures before shipping
  the allowlist enforcement.
- **`pub(crate)` demotion churn (0.4).** Surfacing the boundary may
  reveal that something we thought was internal is actually used by an
  integration test. Each such case is a useful signal but adds rework.

## open questions

These remain open and shape phase 0 in directly visible ways. The
recommendations are mine — confirm or override before implementation
starts on the affected sub-phase.

### 1. Unify `db::SearchHit` and `tools::search::SearchHit` into a single retrieval-layer `RetrievedHit`?

Today both structs exist with overlapping shapes; `tools::search` maps
DB-shape into wire-shape. Unifying clarifies ownership but adds a
rename to the diff.

**Recommendation:** unify. The source doc explicitly calls out this
duplication and the layering is correct (retrieval owns the type, DB
populates it, tools maps to wire). Land it as a separate commit at the
end of 0.2 to keep the diff readable.

### 2. How should newtypes carry the `tool` discriminator for error messages (0.3)?

Three options:

a. **Builder per tool**: `Profile::try_new(tool, value)`. Verbose but
   error messages keep tool context.
b. **Drop tool context** from errors. Cleaner `TryFrom`; less specific
   errors.
c. **Per-tool wrapper module** that bakes the tool name in. Clean call
   sites but reproduces the validate-module shape we're eliminating.

**Recommendation:** (a). Honest about the contextuality of construction;
acceptable verbosity (max ~5 newtypes per handler); a future
`#[tool(name="...")]` derive can shrink it later. (b) is a regression
in error quality; (c) is the original problem.

### 3. Do newtypes propagate to `db.rs`, or stop at the handler boundary (0.3)?

If `db::store_memory` takes `&Profile`, type guarantees flow all the
way down — but `db.rs` is intended to be pure SQL with no domain
knowledge.

**Recommendation:** stop at the handler. `db.rs` is fetch/persist;
construction proved validity; passing `&str` downstream is fine. Bonus:
this is what the engine split (phase 1) implies — the engine's `db.rs`
and `ops/*` should remain decoupled.

### 4. Embedder repo location (phase 2).

Same repo as a new workspace crate, or separate repo with its own
version cadence?

**Recommendation:** decide at phase 2 kickoff, defaulting to the chitta
repo unless there's a versioning reason to split. (Carried over from
the original aion-share doc.)

### 5. Windows postgres install path (phase 3).

EDB installer with silent mode? Scoop? Ship a small native installer
that wraps the EDB MSI?

**Disposition:** decide during phase 3 in the aion repo. Not a chitta
concern.

### 6. Migration ownership when shared.

If a user runs both manas and aion on the same postgres instance, does
each consumer's chitta binary run its own migrations against its own db?

**Answer (carried over):** yes, by default — separate databases mean
separate migration histories. Document this clearly.
