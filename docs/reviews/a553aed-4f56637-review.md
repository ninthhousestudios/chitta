# Code Review: a553aed + 4f56637

**Feature:** External refs, soft-delete, derivations, and `/ingest` extraction endpoint  
**Reviewer:** Codex  
**Date:** 2026-05-06

## Feature Summary

These commits extend `memories` with `external_refs` JSONB and `invalidated_at` soft-delete state, add a `derivations` lineage table, thread the new fields through store/get/list/search/update/delete, and add an authenticated HTTP `/ingest` endpoint backed by an in-process `mpsc` queue and extraction worker. The ingest worker splits text, filters narration, embeds candidate chunks, classifies them against anchor embeddings, and stores accepted memories with generated idempotency keys.

Verification run:

- `env -u RUSTC_WRAPPER cargo test` passed.
- `env -u RUSTC_WRAPPER cargo clippy --all-targets --all-features` passed with warnings.
- `cargo fmt --check` failed repo-wide; several touched files would be reformatted.

## Critical Issues

1. **[High] Soft-delete breaks idempotent re-store after deletion.**  
   `migrations/0001_init.sql:33` keeps the global unique index on `(profile, idempotency_key)`, while reads now hide deleted rows through `invalidated_at IS NULL` (`src/db.rs:141-145`). After `delete_memory` sets `invalidated_at` (`src/db.rs:226-239`), a later store with the same `(profile, idempotency_key)` will pass the preflight lookup, pay for embedding, hit the unique constraint, then fail with `"unique violation without recoverable row"` because the conflict recovery lookup also filters deleted rows (`src/db.rs:111-117`). Either keep deleted rows as idempotency replays intentionally and fetch tombstones explicitly, or replace the old unique index with a partial active-row unique index:
   `CREATE UNIQUE INDEX ... ON memories(profile, idempotency_key) WHERE invalidated_at IS NULL`.

2. **[High] `/ingest` does not actually split normal multi-sentence lines.**  
   `split_sentences` only pushes when a complete line ends with punctuation (`src/ingest.rs:315-347`). A paragraph like `"A. B. C."` stays one chunk; the test at `src/ingest.rs:383-388` codifies that broken behavior. This undermines the advertised pipeline and can cause long paragraphs to exceed the embedder token limit, after which the worker silently drops the whole chunk (`src/ingest.rs:203-208`). Split within lines using `char_indices`/`split_inclusive` plus abbreviation/URL guards, and add tests expecting multiple sentence chunks from one line.

3. **[High] `/ingest` bypasses the tool validation contract.**  
   The handler only checks empty text and a 1 MB byte cap (`src/ingest.rs:66-77`). It accepts arbitrary `profile`, `source`, `project`, and `max_importance`, then turns `source` and `project` into tags (`src/ingest.rs:224-239`, `src/ingest.rs:306-312`). This bypasses `validate::profile`, `validate::tags`, memory-type constraints, and any source/project limits used by the MCP tools. A bearer-protected HTTP endpoint is still a write path; validate at the boundary before enqueueing, or convert the ingest item to a validated internal type.

4. **[Medium] Accepted `/ingest` requests can be lost with no observable state.**  
   The endpoint returns `202 Accepted` once an item enters the in-memory channel (`src/ingest.rs:89-102`), but the worker can later skip embed failures, DB failures, or all low-similarity chunks while only logging at debug/warn (`src/ingest.rs:203-250`). The queue is also non-durable and the spawned worker handle is not monitored (`src/main.rs:404-411`). If this endpoint is meant for hooks where occasional loss is acceptable, document that contract. If not, add a persisted job table, queue status endpoint, or at least a dead-letter/error metric.

5. **[Medium] Derivations are added as schema and DB helpers but have no profile/read-model integration.**  
   The table references memories by UUID only (`migrations/0009_derivations.sql:5-12`), and helper queries do not consider whether source/derived memories are soft-deleted (`src/db.rs:650-705`). Since deletion is now soft, `ON DELETE CASCADE` will rarely run. Future APIs around derivations need profile-aware access checks and active-row filtering, or lineage can outlive the visible memories it describes.

## Idiomatic Improvements

1. **Use typed external refs instead of raw JSON values.**  
   `StoreArgs`, `UpdateArgs`, outputs, and `MemoryRow` expose `external_refs: Option<serde_json::Value>` (`src/tools/store.rs:50-53`, `src/tools/update.rs:45-47`, `src/db.rs:33`). A `Vec<ExternalRef>` plus a `RefKind` enum would give stronger Rust types, better generated schemas, less manual validation, and fewer runtime shape checks.

2. **Introduce input structs for DB writes and updates.**  
   `MemoryRow` is used both as a selected row and as an insert input (`src/db.rs:79-82`), forcing callers to manufacture fields like `invalidated_at: None`. `update_memory` now has 11 parameters (`src/db.rs:180-192`), and clippy flags it. Use `NewMemory` and `MemoryPatch` structs to separate persistence concerns and make adding fields less error-prone.

3. **Precompute anchor norms for classification.**  
   `cosine_similarity` recalculates both norms for every chunk/anchor pair (`src/ingest.rs:279-286`), even though anchor embeddings are static after startup. Store `norm` in `AnchorEmbedding` and compute only the chunk norm once per chunk.

4. **Avoid unnecessary per-item allocations in ingest helpers.**  
   `is_narration` lowercases the whole candidate string (`src/ingest.rs:289-291`), and `make_idempotency_key` formats each digest byte through a tiny allocation pipeline (`src/ingest.rs:294-303`). These are not hot enough to be urgent, but `eq_ignore_ascii_case` prefix checks and `hex::encode`/digest lower-hex formatting would be cleaner.

5. **Keep dependency versions aligned.**  
   `Cargo.toml` adds `sha2 = "0.11.0"`, while `sqlx` already pulls `sha2 0.10.9`; the lockfile now contains both digest stacks. Unless `0.11` is needed, use `sha2 = "0.10"` to avoid duplicate crypto dependencies.

6. **Add targeted tests for the new behavior, not just shape changes.**  
   Existing tests mostly add `external_refs: None`. Add integration coverage for store/get/search/update with non-empty refs, delete-then-store with the same idempotency key, `/ingest` validation rejection, and a sentence-splitting case with multiple sentences on one line.

## The Slop List

1. **Unused API field:** `max_importance` is accepted, defaulted, queued, and never read (`src/ingest.rs:33-39`, `src/ingest.rs:54`, `src/ingest.rs:86`). Remove it or implement the intended threshold.

2. **Dead derivation helpers:** `DerivationRow`, `insert_derivation`, `get_derivations_for`, and `get_derived_from` are not referenced anywhere outside `src/db.rs` yet (`src/db.rs:641-705`). If this is scaffolding for a near-term API, add tests now; otherwise defer until the feature is actually wired.

3. **Formatting drift:** `cargo fmt --check` wants changes in touched files, including `src/ingest.rs`, `src/db.rs`, `src/main.rs`, and `tests/integration.rs`. The most visible new slop is misindented `external_refs: None` entries in integration tests around `tests/integration.rs:221`, `tests/integration.rs:560`, and similar blocks.

4. **Stale comments/tests:** `delete_memory_removes_row` in `tests/integration.rs:797` still names hard-delete semantics even though the implementation now soft-deletes. Update names/comments to `soft_delete_hides_row` or similar.

5. **Docs not updated:** `external_refs`, soft-delete semantics, derivations, and `/ingest` are not reflected in `README.md` or `docs/official/*` according to the current tree search. New public API surface should be documented alongside the tool reference.

6. **Clippy warning left in touched path:** Adding `external_refs` pushed `db::update_memory` further into `too_many_arguments` territory (`src/db.rs:180-192`). This is now visible in clippy output and should be cleaned up with a patch struct.
