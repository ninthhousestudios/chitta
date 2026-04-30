# aion-share refactor

Status: planned
Date: 2026-04-26

Refactor plan to prepare chitta-rs for use as a shared subsystem
between manas (developer cognition os) and aion (astrologer's
operating system, public ship target).

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
(plugin manifest, ui, voice capture pipeline) lives in the aion
repo and is tracked there.

## decisions

### shared via engine + server split

Two crates in a Cargo workspace:

- **chitta-engine** — storage, embedder, retrieval, validation,
  envelope, error, config. No MCP, no transport, no I/O surface
  besides the database.
- **chitta-server** — thin MCP wrapper over the engine. rmcp tool
  router, stdio + streamable-http transports, main.rs binary.

One implementation. Two deployments:

| consumer | how chitta runs | db location |
|---|---|---|
| manas | `chitta-rs serve` (stdio), claude code is the client | dev's postgres, dedicated database |
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
on the same machine can share a postgres instance via separate
databases.

### memory_type → deployment-configured allowlist

Current state: the DB column is plain `text` (migration 0005); the
tool layer hard-codes `VALID_MEMORY_TYPES = ["memory", "observation",
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
behavior change.

### embedder extracted as a sidecar service

When aion lands, two chitta processes (manas + aion) and aion's
chart-db plugin all need the same BGE-M3 model. Three processes
each loading 1.6GB is unviable on the 8-16GB laptop target.

Extract the embedder into a small sidecar:

- separate process, autostart with the host
- tiny protocol over unix socket: `{text, mode: dense|hybrid}` →
  `{dense: [f32; 1024], sparse: {token: weight}}`
- idle timeout to unload model and free RAM (per the manas memory)
- chitta-engine speaks to the sidecar via the embedder client trait
  it already has internally; the in-process impl stays available for
  test/single-process deployments

Repo location: TBD. Could live in chitta's repo as a new crate, or be
its own repo (candidate name: `pratyaksha` — "direct perception").
Defaulting to chitta's repo unless there's a reason to split.

## sequencing

```
A. engine + server crate split   ── ~2 days, no behavior change
B. memory_type → allowlist        ── hours; principles revision PR first
   ↓
   v0.0.3 lands on postgres (hybrid retrieval) — unchanged
   ↓
C. embedder service extraction   ── ~1 week
D. aion install engineering       ── in aion repo: bundle chitta-server,
                                     postgres setup script, plugin manifest,
                                     notebook ui, voice capture, citations
```

A and B are pure prep — they can land now without disturbing v0.0.3.
C blocks aion's first usable build because aion + manas can't both
hold the model. D depends on A, B, and C all being stable.

## work items

### A. engine + server crate split

1. Convert `chitta-rs` to a Cargo workspace.
2. New crate `chitta-engine`. Move:
   - `src/config.rs` → `chitta-engine/src/config.rs`
   - `src/db.rs` → `chitta-engine/src/db.rs`
   - `src/embedding.rs` → `chitta-engine/src/embedding.rs`
   - `src/envelope.rs`, `src/error.rs`, `src/retrieval.rs` → engine
   - `src/tools/validate.rs` → `chitta-engine/src/validate.rs`
   - `src/tools/{store,get,search,update,delete,list,health}.rs` →
     `chitta-engine/src/ops/*`. Strip rmcp `Args` schema types — those
     stay server-side. Engine functions take plain rust args, return
     plain rust results.
3. New crate `chitta-server`. Move:
   - `src/mcp.rs` → `chitta-server/src/mcp.rs` (the rmcp wrapper)
   - `src/main.rs` → `chitta-server/src/main.rs`
   - `*Args` schema types stay here, deserialize from rmcp tool inputs,
     call into engine, wrap engine output into rmcp responses
4. `Cargo.toml` for the workspace; per-crate `Cargo.toml`s with
   minimal feature sets.
5. CI matrix: build engine alone (no rmcp); build server (engine +
   rmcp); run all integration tests.

Acceptance: `cargo build -p chitta-engine` succeeds with no rmcp
dependency in the engine's tree. All existing tests pass.

### B. memory_type → allowlist from config

1. Land a principle-revision PR updating `docs/principles.md` to
   reflect the per-deployment vocabulary policy.
2. Add `Config::allowed_memory_types: Vec<String>` populated from
   `CHITTA_MEMORY_TYPES` env var (comma-separated). Default = current
   five.
3. `validate::memory_type` reads from config (passed in or available
   via the tool context), not the static `VALID_MEMORY_TYPES`.
4. Update the affected tests in `validate.rs` to drive both default
   and custom allowlist cases.
5. Document the env var in the official version docs (or wherever
   config keys are listed).

Acceptance: a chitta-server started with
`CHITTA_MEMORY_TYPES=foo,bar` accepts `memory_type: "foo"` and rejects
`memory_type: "memory"`. A server started with no env var accepts the
five legacy types and rejects unknowns.

### C. embedder service extraction

1. New crate `chitta-embedder` (or `pratyaksha` if it gets its own
   repo). Hosts an embedder pool and a unix-socket server.
2. Wire protocol — minimal length-framed JSON or msgpack:
   - `Embed { input: Text(String) | Image(bytes), mode: "dense" | "hybrid" }`
   - response: `{ dense: [f32], sparse: Option<Map<String, f32>> }`
   - **Multimodal from day one.** The input enum accepts images even if
     the initial model (BGE-M3) only handles text. Grantha (document
     intelligence layer atop smriti) will send page images when local
     multimodal embedding models mature. Designing the interface now
     avoids retrofitting every consumer later. The server returns an
     error for unsupported modalities until a multimodal model is loaded.
3. Client crate (used by chitta-engine and eventually chart-db) that
   speaks the protocol; falls back to in-process embedder if the
   socket is unset (test/single-process deployments).
4. Idle eviction: unload model after N minutes idle, reload on next
   request. Tunable.
5. Service has its own binary + manifest entries for both manas and
   aion to spawn it.
6. Update chitta-engine to use the client by default when
   `CHITTA_EMBEDDER_SOCKET` is set; keep the in-process path for
   tests and small deployments.

Acceptance: two chitta-server processes pointing at the same
embedder socket can both `store_memory` concurrently, only one
BGE-M3 model is resident in RAM, and idle eviction works.

### D. aion install engineering

Lives in the aion repo, not here. Tracked there. Touchpoints with
chitta:

- chitta-server bundled as an aion plugin manifest entry (stdio,
  autoStart, db url from config)
- aion's installer runs postgres setup (detect, install if needed,
  create role, create `aion_chitta` db, run chitta migrations)
- aion ships the embedder sidecar as a separate manifest entry,
  exporting its socket path; chitta and chart-db both read it from env

## what this is NOT

- **Not a storage backend abstraction.** Postgres stays. SQLite is not
  added. The `Repo` trait idea is explicitly deferred — revisited only
  if windows install pain proves unacceptable in field testing.
- **Not a v0.0.3 reroute.** v0.0.3 (hybrid retrieval + agent-native
  quality) lands on the current single-crate codebase. The split work
  starts after that ships, or in a parallel branch that rebases.
- **Not a public/multi-user pivot.** Profiles remain the only isolation
  primitive (principle 7). Each install is single-postgres-instance,
  one db per consumer (manas vs aion), profiles within for
  client/topic scoping.
- **Not a chitta UI project.** Aion will build its notebook ui on top
  of chitta's MCP tools. Chitta itself stays headless.

## open questions

- **Embedder repo location.** Same repo as a new workspace crate, or
  separate repo with its own version cadence? Decision deferred to
  step C kickoff.
- **Windows postgres install path.** EDB installer with silent mode?
  Scoop? Ship a small native installer that wraps the EDB MSI? Decided
  during step D in the aion repo.
- **Migration ownership when shared.** If a user runs both manas and
  aion on the same postgres instance, does each consumer's chitta
  binary run its own migrations against its own db? (Yes, by default —
  separate databases mean separate migration histories. Document
  this clearly.)
- **Type-weights config interplay.** `Config::search.type_weights` is
  keyed by memory_type. With deployment-configured allowlists, the
  weights should validate against the allowlist at config load.
