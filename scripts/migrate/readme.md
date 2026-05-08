# chitta-migrate

Throwaway CLI for migrating memories from the old chitta DB to the new v0 schema.

## Flow

```
export  →  hand-edit  →  dry-run  →  seed
```

### 1. Export

Reads all memories from the source DB and writes one JSONL line per row.
Adapts to different schema versions (old Python chitta, chitta-rs, new chitta v0).
Embeddings are excluded — seed re-embeds everything.

```sh
chitta-migrate export \
  --source "postgresql://josh:ogham@localhost/chitta_old" \
  --out old.jsonl
```

### 2. Hand-edit

Open `old.jsonl` in your editor. Each line is a self-contained JSON object.

Common edits:
- Delete rows that don't belong in the working model (project artifacts go to yojana)
- Set `memory_type` (default is `observation`; upgrade to `decision`, `trait`, etc.)
- For `decision` rows: add `metadata.rationale` and `metadata.rejected_alternatives`
- For `episode` rows: add `derivations` array
- Add `applies_to_*` facets for retrieval scoping

### 3. Dry-run

Validates every row through the same validators as `store_memory`. Reports
rejections with principle-8 error text (tool, constraint, next action). Does
not touch the database.

```sh
chitta-migrate seed --from old.jsonl --dry-run
```

Fix rejections in the JSONL file and re-run until clean.

### 4. Seed

Inserts validated rows into the target DB (from `DATABASE_URL`). Each row
is re-embedded with the current model and tagged `seed:2026-05`.

```sh
chitta-migrate seed --from old.jsonl
```

Idempotent: re-running won't duplicate rows (same `idempotency_key` returns
the existing row).

## Fixture

`fixture.jsonl` contains 7 test rows (5 valid, 2 deliberately invalid) for
smoke-testing the validator pipeline.

```sh
chitta-migrate seed --from scripts/migrate/fixture.jsonl --dry-run
```

Expected output: 5 ok, 2 rejected (decision without rationale, episode without derivations).
