# chitta-rs — architecture deepening opportunities

Status: findings (not yet planned)
Date: 2026-04-29

Architectural review of chitta-rs, informed by the domain model in `docs/manas-architecture.md`, the principles in `docs/principles.md`, and the aion-share decision (chitta memory `019dcacd-c1f1`). Uses the vocabulary from the improve-codebase-architecture skill: module, interface, depth, seam, locality, leverage.

---

## 1. Domain newtypes to replace validate.rs

**Files:** `src/tools/validate.rs` (354 lines), all 7 tool handlers

**Problem:** `validate.rs` is a shallow module — 12 free functions whose interface is nearly as complex as their implementation. Each function takes a raw `&str` plus a `tool: &'static str` discriminator, making validation opt-in at call sites. The deletion test confirms shallowness: deleting `validate.rs` wouldn't concentrate complexity anywhere — it would redistribute the same guard clauses across each handler, where they already structurally live (8 sequential `validate::foo(TOOL, &args.bar)?` calls in `store.rs` alone).

The `tool: &'static str` parameter on every function is the telltale: the validator doesn't know its own context and must be told.

**Solution:** Replace with newtypes — `Profile(String)`, `Tags(Vec<String>)`, `MemoryType(String)`, `IdempotencyKey(String)` — that enforce constraints at construction via `TryFrom`. The `tool` discriminator disappears because errors become self-contextualizing. `validate.rs` either vanishes or shrinks to the newtype definitions.

**Benefits:** Locality — each constraint lives with its type, not scattered across N callers. Leverage — every tool handler gets correctness by constructing the type, not by remembering to call the right guard. Tests shift from "did the handler call `validate::profile`?" to "can I construct an invalid `Profile`?" — which is the right question.

**Aion-share alignment:** The planned configurable memory-type allowlist (aion-share decision item 3) directly motivates replacing the hardcoded `VALID_MEMORY_TYPES`. A `MemoryType` newtype that validates against config at construction is the natural implementation.

---

## 2. Consolidate retrieval into a deep retrieval module

**Files:** `src/retrieval.rs` (193 lines), `src/tools/search.rs` (376 lines), `src/db.rs` (lines 386-437)

**Problem:** Retrieval logic has poor locality — it's split across three files:
- The recency scoring formula is **duplicated** (`db.rs:426-437` and `retrieval.rs:121-128`) — identical math, independently maintained.
- `dedup_by_field` and `apply_budget` are retrieval post-processing algorithms living in `tools/search.rs`, a tool handler.
- `apply_type_weights` is called inconsistently: the hybrid path applies it internally (`retrieval.rs:131`), the non-hybrid path applies it externally (`tools/search.rs:155`). A caller must know which path already applied type weights — the interface leaks implementation details.
- `min_similarity` is silently dropped in the hybrid path (`retrieval.rs:37` passes `0.0` to db).

A change to retrieval scoring requires understanding all three files.

**Solution:** Move all scoring/ranking/dedup/budget logic into `retrieval.rs`. `db.rs` does SQL fetching only — no post-SQL scoring. `search.rs` becomes a thin dispatcher: validate, embed, call retrieval, envelope. `retrieval.rs` becomes a deep module with a small interface (`search(query, config) -> Vec<SearchHit>`) hiding significant behavior (RRF fusion, recency, type weights, dedup, budget fitting).

**Benefits:** Locality — fix a scoring bug in one place, not three. Leverage — the retrieval interface becomes the test surface. You can test the full retrieval pipeline (including dedup, budget, recency) without going through MCP, which is currently impossible since `dedup_by_field` and `apply_budget` are only reachable via `tools::search::handle`. The `search.rs` handler drops from 376 lines to ~80.

---

## 3. Extract admin commands from main.rs

**Files:** `src/main.rs` (lines 156-325)

**Problem:** `run_replay` (90 lines, cognitive complexity 8) and `run_backfill` (77 lines, cognitive complexity 12 — the highest cyclomatic complexity in the codebase at 19) are domain-touching functions stranded in the binary entry point. They use the full library API, contain loops with retry logic, and make domain decisions — but they're untestable from the library side because `main.rs` is not part of the lib crate. `main.rs` has `fan_in: 0` — nothing depends on it, nothing tests it.

**Solution:** Move to `src/admin.rs` as `pub` functions. `main.rs` dispatches to them.

**Benefits:** Locality — admin concerns in one module. Testability — these functions become callable from integration tests. The deletion test: deleting `run_backfill` from `main.rs` into `admin.rs` doesn't change caller complexity (there's only one caller) but reveals it to the test harness. This is a seam that currently doesn't exist.

**Aion-share alignment:** The engine/server crate split will need to decide where admin commands live. Having them already extracted into a library module makes the split cleaner — they move with `chitta-engine`, not `chitta-server`.

---

## 4. Invert the config to validate dependency

**Files:** `src/config.rs` (line 144), `src/tools/validate.rs` (line 192)

**Problem:** `config.rs` imports `crate::tools::validate::VALID_MEMORY_TYPES` to parse type-weight configuration. This is a dependency inversion — config is a low-level module loaded at startup before tools are ever invoked, yet it depends on a tools-layer module. The canonical direction is tools to config. This coupling will break when the engine/server crate split happens: `VALID_MEMORY_TYPES` will need to be in `chitta-engine`, but if it stays in `tools::validate`, it's in the wrong crate.

**Solution:** Move the type list to where config can import it — either into `config.rs` itself (as a config-driven allowlist, per the aion-share plan), or into a dedicated `src/types.rs` module that both config and tools import.

**Benefits:** Correct dependency direction. The engine/server split becomes mechanically simpler. Locality — the type vocabulary and its validation live together.

---

## 5. Curate lib.rs as a real interface

**Files:** `src/lib.rs` (13 lines)

**Problem:** `lib.rs` is 8 `pub mod` declarations — a flat namespace listing, not an interface. Every internal type is externally visible. The blast radius of 19 is not depth; it's the absence of information hiding. Functions like `db::is_unique_violation`, `db::fetch_sparse_embeddings`, and `db::PG_UNIQUE_VIOLATION` are implementation details that callers can reach because nothing stops them. The module's interface is its implementation — the definition of shallow.

**Solution:** Selective `pub use` for the genuine public API. Make internal plumbing `pub(crate)`. Define the crate boundary as a real seam.

**Benefits:** Leverage — callers depend on a curated interface, not internal plumbing. Internal refactors don't break external callers. This is the foundational seam for the engine/server crate split.

**Aion-share alignment:** The engine/server split literally requires deciding what the engine crate exports. Doing this now means the split is a `Cargo.toml` change, not an interface design exercise under time pressure.

---

## 6. Consolidate thin tool handlers

**Files:** `src/tools/delete.rs` (54 lines), `get.rs` (73 lines), `health.rs` (55 lines), `list.rs` (103 lines)

**Problem:** Four files each containing 1 struct pair + 1 trivial handler. These are shallow modules — their interface (a function taking `Args` and returning `Output`) is approximately as complex as their implementation (2 validate calls, 1 db call, 1 map). The file boundaries provide no encapsulation: everything is `pub`, re-exported flat from `tools/mod.rs`, and the implementations are direct `db::` calls. The deletion test: merging any of these into a neighbor changes nothing about how callers interact with them.

**Solution:** Merge into 1-2 files (`tools/crud.rs` for get/delete/list, keep `health.rs` if preferred). The three substantive tool handlers (`store.rs`, `update.rs`, `search.rs`) stay separate.

**Benefits:** Reduced navigation cost without losing clarity. The current 7-file structure adds 4 navigation hops for modules that hide nothing. Consolidation is honest about the depth these handlers actually have.

---

## Additional observations (not full candidates)

- **`error.rs` mixes startup errors with runtime errors.** `MissingConfig` is exclusively a startup error but lives in the same enum as `ContentTooLong` and `NotFound`. Different audiences (operator vs. agent) handled identically.
- **`embedding.rs::embed_full` is 200 lines doing 5 phases** fused inside a `spawn_blocking` closure, making sparse extraction logic unreachable for unit testing. The external interface is good (small and deep), but the internal structure prevents seam injection.
- **`db.rs::update_memory` has a 10-parameter signature.** The MemoryRow schema leaks through optional positional parameters. An update-request struct would be cheaper to evolve.
- **`mcp.rs` has 7 copy-pasted serialization closes.** `serde_json::to_string_pretty(&out).map_err(json_to_rmcp)` repeated for every tool. A one-line helper eliminates the noise.
- **`SearchHit` is defined in both `db.rs` and `tools/search.rs`** with different shapes. Both named `SearchHit`. Correct layering, confusing naming.
- **`MAX_TOKENS` in `embedding.rs` and the hardcoded "8192" string in `error.rs`** will drift silently if the model changes.
- **Query log functions in `db.rs`** (`read_query_log`, `insert_query_log`, `QueryLogEntry`) serve a replay/research subsystem, not the live path. Different reason to change than the rest of db.rs.

---

## Sequencing notes

Candidates 1 (newtypes) and 4 (dependency inversion) are closely related and should be done together — the newtype for `MemoryType` naturally absorbs the `VALID_MEMORY_TYPES` constant.

Candidate 5 (curate lib.rs) is a prerequisite for the engine/server crate split from the aion-share plan. It could be done independently at any time.

Candidate 2 (retrieval consolidation) is the highest-leverage change for retrieval quality work — any future scoring experiment will be cheaper with a single retrieval module.

Candidate 3 (extract admin) is low-risk, low-effort, and unblocks testability for backfill/replay.
