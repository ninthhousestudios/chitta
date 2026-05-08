# PRD — chitta working-model pivot (rename + clean break)

Status: draft, pending Josh approval
Date: 2026-05-08
Yojana task: chitta/13
Decision record (direction): chitta memory `019e0453-ad7d-7912-92bb-e36101158f55`
Decision record (this PRD's scope): chitta memory `019e06fb-2980-73e1-83b0-531587e26702`
Direction doc: `docs/working-model-pivot.md`
ADR: `docs/adr/0001-working-model-framing.md`
Glossary: `CONTEXT.md`

## Problem Statement

Today's chitta hosts at least three different shapes of content under one contract: project / work artifacts, domain knowledge, and observations about Josh-as-a-person. Three shapes share a schema, a tool surface, and a profile namespace. The result is the same kitchen-sink memory store the chitta-rs rewrite was supposed to escape — just smaller. Agents have no stable way to load "who is Josh and how does he work" at session start; every session restarts from zero on the human-in-the-loop.

Project memory has a home (yojana). Code, files, and documents have homes (sutra, smriti, kosha). Knowledge graphs will have a home (vidya). Nothing models the human in the loop.

## Solution

Narrow chitta's contract to **a working model of Josh** — a stored, evolving model of what he values, how he works, what he prefers, and what mental models he uses, available to every agent in manas across every domain.

Three concrete user-visible changes:

1. **Always-on profile.** A small slice of the working model (~30 entries: top traits, values, preferences, patterns, mental models) is loadable at session start with no query. Agents act consistently with Josh from the first turn.
2. **Context-faceted retrieval.** Skills can ask "what's relevant about Josh given this context (skill, project, domain, situation)" and get a useful answer without authoring a search query.
3. **Synthesis with supersession.** Raw observations / decisions / episodes accumulate. /reflect synthesizes them into consolidated traits/values/patterns/preferences/mental_models. Contradicting evidence supersedes; old isn't deleted, just stops surfacing.

Underneath: a clean schema break (one fresh migration, no in-place upgrade), the `chitta-rs` → `chitta` rename, and a tighter tool surface that keeps writes fast and enrichment lazy (principle 3).

## User Stories

### Loading the working model

1. As a Claude Code session, I want to load Josh's always-on profile at session start with one tool call, so that I can act consistently with him from the first turn without first searching.
2. As a Claude Code session, I want the always-on profile to be ordered by an effective score that combines confidence and recency, so that traits Josh has reinforced recently surface ahead of stale ones.
3. As a skill (e.g. `review`), I want to retrieve "what is relevant about Josh given this context" by passing facets like `{skills: ["review"], domains: ["rust"]}`, so that I can scope retrieval without authoring a query string.
4. As an agent, I want default retrieval to search only consolidated memories (no raw observations/episodes/decisions), so that responses are grounded in synthesised claims rather than noisy raw material.
5. As an agent doing a deep retrospective, I want to opt into raw-layer search via `include_raw=true`, so that I can find the specific moment Josh said something.

### Writing to chitta

6. As the `done` skill at session end, I want to write raw `observation` rows for noteworthy moments, then write a single `episode` row whose `derivations` point at those observations, so that /reflect has a session-bounded unit to synthesise from.
7. As an agent during a session, I want to proactively store observations about Josh's preferences/values/style without permission, so that the working model accumulates signal from real interactions (per existing CLAUDE.md instructions, with profile updated to `josh`).
8. As an agent capturing a decision Josh made, I want chitta's `decision` writes to be hard-validated — I must supply rationale and at least one rejected alternative, otherwise I'm told to either supply them, demote to `observation`, or route to yojana.
9. As an agent storing a project-artifact decision ("we picked Postgres for chitta"), I want the system to make it obvious that this belongs in yojana, not chitta — chitta only takes decisions that carry working-model signal.
10. As any caller, I want every write to be idempotent on `(profile, idempotency_key)`, so that retries don't duplicate (existing principle 6).

### /reflect synthesis

11. As Josh, I want to manually trigger /reflect when I want synthesis to run, so that I control when LLM-heavy work happens.
12. As /reflect, I want to read raw rows across all time (not windowed), extract candidate consolidated claims, cluster them by similarity, and emit a consolidated row only when a cluster meets the threshold (≥5 corroborating rows AND ≥2 distinct days AND ≥1 source <90 days old).
13. As /reflect, when I detect a candidate cluster's claim contradicts an existing consolidated row, I want to emit the new row with `superseded_by` set on the old one and write a meta-observation about the change, so that the audit trail of how Josh's working model evolved is preserved.
14. As /reflect, I want to populate the `derivations` table with one row per (consolidated, source) link, so that any consolidated memory can be traced back to the raw evidence that produced it.

### /agree, /disagree

15. As Josh, I want to invoke `/agree <memory-id>` (and `/disagree <memory-id>`, optionally with a correction) to give explicit feedback on a working-model claim, so that the system has high-signal reinforcement events.
16. As `/agree`, I want to bump a consolidated row's `confidence` by +0.05 (capped at 1.0), increment `reinforcement_count`, set `last_reinforced_at = now`, and write a raw-layer feedback row pointing back at the memory.
17. As `/disagree`, I want to drop a consolidated row's `confidence` by 0.10 (floored at 0.0), update `last_reinforced_at`, write a raw-layer feedback row, and (if a correction string is supplied) write a separate raw-layer observation carrying the correction text — so that /reflect picks the correction up as contradicting evidence on the next run without auto-superseding.
18. As /reflect, I want to identify "memories needing revisit" by querying for consolidated rows with `feedback:disagree`-tagged raw rows written since my last run, so that no separate `pending_review` column is needed.

### Renaming and migration

19. As anyone reading the manas docs or running the binary, I want the subsystem consistently named `chitta` (no `-rs` suffix), so that naming matches the rest of manas.
20. As Josh seeding the new DB, I want a one-shot tool that exports the old DB to JSONL, lets me hand-edit which rows survive (and how they map to the new schema), and applies them to the new DB with the same write-time validation as `store_memory`.
21. As Josh post-seed, I want the old DB renamed to `chitta_archive_2026_05` and made read-only, so that it's clearly identifiable as historical material with no risk of accidental writes.
22. As anyone deleting old project-artifact decisions from the seed JSONL, I want to know the archive remains accessible — those rows are not lost, just not part of the working model.

### Validation and errors

23. As any caller hitting a validation error, I want the error to name the tool, the offending argument, the constraint, and the next action (per principle 8), so I can either fix the call or route to the right subsystem.

## Implementation Decisions

### Subsystem framing

- chitta is the **working model of Josh** — what he values, how he works, what he prefers, what mental models he uses. "Personality" is dropped from the vocabulary (ADR-0001). The Sanskrit *citta* (field of impressions) fits this framing; the implementation had drifted from the name and is being brought back to it.
- Subject scope is Josh only in v0. No `subject` column. A second subject would warrant a separate DB.
- chitta does not own project-artifact decisions ("we picked Postgres for chitta") — those belong to yojana. chitta takes decisions only when they carry working-model signal AND satisfy the rationale + rejected_alternatives contract. Routing is the caller's job; chitta does not deduplicate against yojana.
- Knowledge graphs (vedic astrology entities, generic concept entries) belong to vidya, a planned subsystem peer to chitta. Domain-knowledge rows in the old DB are not migrated by this PRD's seed step; vidya will mine the archive when ready.

### Memory taxonomy

Three layers, eight memory types:

- **Raw / episodic** (`observation`, `episode`, `decision`) — append-only, immutable, written by harness/skills. Volume: thousands → tens of thousands.
- **Consolidated / semantic** (`trait`, `value`, `pattern`, `preference`, `mental_model`) — written exclusively by /reflect, mutated only by `/agree` / `/disagree` / supersession. Volume: tens → low hundreds.
- **Profile / always-on** — derived view, not a stored type. The top-N consolidated rows by effective score, computed at read time.

### Profile namespace

- One profile in v0: `josh`. No `josh-work-<project>` split. Project context lives on the row via `applies_to_projects[]`, not as a separate profile (per principle 7 — profiles stay clean for future multi-tenant). Adding a second profile is a deliberate decision tied to a new subject.

### Schema (one fresh migration)

The new `chitta` DB starts with a single migration `0001_init.sql`. All v0 columns ship in this one file; no incremental migrations from the old chitta-rs schema apply.

Core columns on `memories`:

- `id uuid PRIMARY KEY`
- `profile text NOT NULL`
- `content text NOT NULL` (verbatim, principle 1)
- `event_time timestamptz NOT NULL`, `record_time timestamptz NOT NULL DEFAULT now()` (bi-temporal, principle 2)
- `idempotency_key text NOT NULL` (principle 6)
- `embedding vector(1024)` (BGE-M3 dense)
- `sparse_embedding` (BGE-M3 sparse — kept; format follows existing chitta-rs implementation)
- `memory_type text NOT NULL` — restricted by CHECK constraint to the eight types above
- `tags text[] NOT NULL DEFAULT '{}'`
- `external_refs jsonb` (typed `[{kind, ref}]` shape, kinds: file, commit, yojana_task, memory, url, session)
- `metadata jsonb` (structured per-type fields — for `decision`: `rationale`, `rejected_alternatives`)
- `applies_to_domains text[] NOT NULL DEFAULT '{}'`
- `applies_to_skills text[] NOT NULL DEFAULT '{}'`
- `applies_to_projects text[] NOT NULL DEFAULT '{}'`
- `applies_to_situations text[] NOT NULL DEFAULT '{}'`
- `superseded_by uuid NULL REFERENCES memories(id)`
- `confidence real NULL` (NULL for raw layer; 0.0–1.0 for consolidated)
- `reinforcement_count int NOT NULL DEFAULT 0`
- `last_reinforced_at timestamptz NULL`
- `invalidated_at timestamptz NULL` (soft delete)

Indexes (initial set):

- HNSW on `embedding` (dense ANN, cosine).
- One GIN per `applies_to_*` facet column (per-facet selectivity).
- GIN on `tags`, `external_refs`.
- Unique partial on `(profile, idempotency_key) WHERE invalidated_at IS NULL` (idempotency for active rows).
- Partial on `(profile, memory_type, record_time DESC) WHERE invalidated_at IS NULL`.
- Partial covering tier-0 candidate set: `(profile, memory_type, confidence DESC) WHERE memory_type IN ('trait','value','preference','pattern','mental_model') AND superseded_by IS NULL AND invalidated_at IS NULL`.

Adjacent tables:

- `derivations (id, derived_id → memories.id, source_id → memories.id, derivation_type text, created_at)` — same shape as today's 0009 migration. Indexed on derived_id, source_id, derivation_type.
- `query_log` — kept (used for replay/regression eval). Schema unchanged from today.

Dropped from current chitta-rs schema (not ported):

- FTS columns/indexes (astrobench showed no lift over dense+sparse+RRF).
- The `memory` generic memory_type (every row gets a real type).
- Any session_summary-as-freeform-blob columns/conventions (replaced by `episode` with derivations contract).

### Validation contracts (hard-rejecting at write time)

- `memory_type=decision`: `metadata.rationale: string` (non-empty) AND `metadata.rejected_alternatives: string[]` (length ≥ 1). Else 400 with: *"decision memory requires `metadata.rationale` and at least one `metadata.rejected_alternatives` entry. Either supply them, demote to memory_type=observation, or route to yojana."*
- `memory_type=episode`: ≥ 1 entry in `derivations` linking the episode to a source memory, written atomically with the episode itself. Else 400 with: *"episode memory requires at least one entry in derivations linking to source observations. Either supply derivations, or use memory_type=observation."*
- `memory_type` value must match the eight-type CHECK; else 400 with the allowed set.
- `applies_to_*` columns must be `text[]`; CHECK constraint enforces.

### Tool surface

Total v0 tools: **10** (existing 7 + 3 new).

Existing (ported with new schema awareness): `store_memory`, `get_memory`, `search_memories`, `list_recent_memories`, `update_memory`, `delete_memory`, `health_check`.

New:

- `get_profile(profile) -> {memories: [...]}` — tier-0. SQL over-fetches top-100 by raw `confidence` against the consolidated-active partial index; app-side `effective_score` computes the decayed score and truncates to top 30.
- `supersede_memory(profile, old_id, new_id, reason)` — marks `superseded_by = new_id` on old_id; new_id may be either an existing memory or supplied as a fresh memory written atomically. Audit trail in `derivations` with `derivation_type='supersedes'`.
- `record_feedback(profile, memory_id, kind: 'agree' | 'disagree', correction?: string)` — see §"/agree, /disagree mechanics" below.

Refined:

- `search_memories` accepts new optional args: `applies_to: {domains?, skills?, projects?, situations?}` (intersection with row facets), `include_raw: bool` (default false → consolidated only). Default search excludes `superseded_by IS NOT NULL` and `invalidated_at IS NOT NULL`. Ranks by `similarity × effective_score` using the same `scoring` module as tier-0.

### Scoring (single source of truth, app-side)

A new `chitta::scoring` module exposes:

```rust
pub fn effective_score(
    confidence: f32,
    last_reinforced_at: Option<DateTime<Utc>>,
    record_time: DateTime<Utc>,
    now: DateTime<Utc>,
) -> f32 {
    let anchor = last_reinforced_at.unwrap_or(record_time);
    let days = (now - anchor).num_days() as f32;
    let half_life_days = 180.0_f32;
    confidence * 0.5_f32.powf(days / half_life_days)
}
```

180-day half-life is a v0 starting value, expected to be tuned against the gold set. Used by tier-0 ordering and by tier-2 ranking (`similarity × effective_score`) — single place to evolve.

### Confidence dynamics

| Event | Effect |
|---|---|
| Hand-seeded row (Phase 3) | `confidence = 0.70` |
| /reflect emission | `confidence = min(0.90, 0.50 + 0.05 × cluster_size)` |
| `/agree` | `confidence = min(1.00, confidence + 0.05)`; `reinforcement_count++`; `last_reinforced_at = now` |
| `/disagree` | `confidence = max(0.00, confidence - 0.10)`; `last_reinforced_at = now` |
| /reflect supersedes | old row's `confidence` frozen as-is; `superseded_by` set; new row gets emission confidence |

Asymmetric agree/disagree (−0.10 vs +0.05) is deliberate — disagreements are rarer and richer signal than agreements.

### /reflect synthesis pipeline

Manual trigger (slash command / skill) in v0. Not auto-run by `/done` or any other flow. Runs against `profile=josh` only.

Pipeline:

1. **Read** all raw rows (`observation`, `episode`, `decision`) for the profile, ordered by `record_time`. All-time, not windowed. Plus all currently-active consolidated rows (for contradiction detection).
2. **Extract** candidate consolidated claims from each raw row via LLM. Each candidate: `{type, claim, source_id}`.
3. **Cluster** candidates by semantic similarity of the claim string (LLM-assisted grouping or embedding clustering).
4. **Emit** a consolidated row plus `derivations` rows for each cluster meeting **all** of:
   - cluster size ≥ 5 source rows,
   - sources span ≥ 2 distinct days (by `record_time::date`),
   - at least 1 source row has `record_time` within the last 90 days.
5. **Detect contradictions**: for each candidate cluster, semantically compare against active consolidated rows. If contradiction detected, emit the new row with `superseded_by` populated on the matching old row and write a raw-layer `mental_model`-tagged observation describing the change.
6. **Pick up disagreement flags**: query `tags @> ARRAY['feedback','disagree']` raw rows written since last /reflect run; treat the targeted memories as supersession candidates (if a contradicting cluster forms, supersede; otherwise leave the confidence drop as the only effect).

Single global threshold; no per-type tuning in v0. Per-type tuning is a post-measurement decision driven by gold-set evaluation.

The /reflect prompt itself (LLM extraction prompt, clustering prompt, contradiction-detection prompt) is iterated in implementation; the PRD specifies the pipeline shape and threshold logic, not exact wording.

### /agree, /disagree mechanics

Two skills, cross-harness, exposed in Claude Code as `/agree` and `/disagree`. Skills accept memory IDs as arguments — the agent (Claude) is responsible for figuring out which IDs to pass from its conversation context. No server-side session state, no magic "last" form in v0 (preserves principle 7).

Both skills call the single `record_feedback` tool. The tool:

- Validates `memory_id` references an active (`superseded_by IS NULL AND invalidated_at IS NULL`) consolidated row in the named profile. Else 400: *"record_feedback applies only to consolidated memories. Memory `<id>` is type=`<observation|decision|episode>`; raw-layer rows do not carry confidence."*
- For `kind=agree`: updates confidence/reinforcement_count/last_reinforced_at as in §Confidence dynamics. Inserts a raw-layer `observation` with content `"Josh agreed with: <quoted memory content>"`, tags `["feedback","agree"]`, external_refs `[{kind: "memory", ref: <memory_id>}]`.
- For `kind=disagree`: updates confidence/last_reinforced_at. Inserts a raw-layer `observation` with content `"Josh disagreed with: <quoted memory content>"`, tags `["feedback","disagree"]`, external_refs as above. **If `correction` is supplied**, also inserts a separate raw-layer `observation` with content equal to the correction text, tags `["correction","contradicts:<memory_id>"]`, external_refs as above. /reflect picks this up on its next run as contradicting evidence.

Returns: `{memory_id, new_confidence, kind, feedback_row_id, correction_row_id?}`.

### Caller-side alignment (must ship with the schema break)

- **`/done` skill**: rewritten. Two-phase. Phase 1: capture observations during the session (existing pattern, profile updated to `josh`). Phase 2: at session end, write a single `episode` row with derivations linking to those observations. `decision` writes are gated by the rationale + rejected_alternatives contract; otherwise route to yojana or demote to observation.
- **`~/CLAUDE.md` "During-Session Observations" section**: profile updated `chitta` → `josh`; decision routing note added (project decisions to yojana, working-model decisions only with contract); decision contract referenced.
- **/reflect trigger**: manual only in v0. No auto-trigger from /done.

### Rename plan (lands with the schema PR)

| Touchpoint | Change |
|---|---|
| `chitta/Cargo.toml` package name | `chitta-rs` → `chitta` |
| Default DB name in `chitta::config` | `chitta_rs` → `chitta` |
| Test DB name | `chitta_rs_test` → `chitta_test` |
| `CHITTA_HOME` env var | unchanged |
| `chitta/README.md` | name + DB references updated |
| `chitta/docs/principles.md` | title + references updated |
| `manas/docs/manas-architecture.md` | chitta section updated post-merge |
| `.env.example` (if any) | DB names updated |

Old DB:

- After seed completes (Phase 3), rename `chitta_rs` → `chitta_archive_2026_05`; revoke write privileges from the application role on the archive.
- Archive kept indefinitely. No deletion plan.

### Migration tooling (throwaway)

A separate `chitta-migrate` CLI (lives in `bench/` or `scripts/`, not in the binary's main surface):

- `chitta-migrate export --source chitta_rs --out old.jsonl` — dumps each old row to one JSONL line.
- (Hand-edit step.) Josh edits `old.jsonl` directly: drops rows that don't carry working-model signal; maps `memory_type` to the new vocabulary; fills `applies_to_*` facets; sets `confidence` (default 0.70).
- `chitta-migrate seed --from old-edited.jsonl --dry-run` — runs the edited rows through the new schema's write-time validation; reports rejections without writing.
- `chitta-migrate seed --from old-edited.jsonl` — applies the rows to the new DB.
- Every seeded row is tagged `seed:2026-05` for cohort identification.

Volume target: ~30 high-signal consolidated rows. Don't migrate everything — the clean break is the point.

What does NOT migrate: project-artifact decisions, generic `memory`-type rows, domain-knowledge rows, old `query_log` entries, old embeddings (regenerated).

### Module layout

| Module | Status | Notes |
|---|---|---|
| `chitta::scoring` | new (deep) | `effective_score(...)`. Pure function. Used by tier-0 and tier-2 ranking. |
| `chitta::validators` | new (deep, split out of current `tools::validate`) | Decision/episode contracts, type enum, applies_to shape, idempotency |
| `chitta::synthesis` | new (deep — the threshold/cluster-decision logic is testable with mocked LLM) | /reflect pipeline core |
| `chitta::tools::store` | refactor | Calls validators; writes raw or consolidated atomically |
| `chitta::tools::search` | refactor | Accepts `applies_to`, `include_raw`; calls scoring |
| `chitta::tools::get_profile` | new | Tier-0 fetch + app-side decay sort |
| `chitta::tools::supersede_memory` | new | Sets `superseded_by`; writes derivation |
| `chitta::tools::record_feedback` | new | Confidence mutation + raw feedback row(s) |
| `chitta::tools::{get,list,update,delete,health}` | port | Schema-aware updates |
| `chitta::config` | modify | DB names |
| `chitta-migrate` | new (throwaway) | Export + seed CLI |
| `~/.claude/skills/done/...` | modify | Two-phase episode writer |
| `~/.claude/skills/agree/...`, `disagree/...` | new | Wrap `record_feedback` |
| `~/.claude/skills/reflect/...` | new (or rewrite) | Manual /reflect trigger |
| `~/CLAUDE.md` | modify | Profile + decision-routing notes |

## Testing Decisions

A good test for chitta exercises external behavior — what callers see — not internal implementation details. Tests use the chitta-rs `tests/contract.rs` and `tests/integration.rs` patterns as prior art.

### Test focus by module

| Module | Priority | What to test |
|---|---|---|
| `chitta::scoring` | High | Decay-curve edge cases (day 0, half-life day, year+); asymmetric agree/disagree cumulative effects; score composition with tier-2 similarity; `last_reinforced_at` null-fallback to `record_time` |
| `chitta::validators` | High | Each contract: `decision` (rationale + ≥1 alternative happy + each rejection path), `episode` (≥1 derivation happy + missing rejection), memory_type enum, applies_to shape, idempotency_key. Both happy paths and rejection-with-actionable-error. |
| Tool handlers (contract layer) | Medium | Existing `tests/contract.rs` pattern extended for `get_profile`, `supersede_memory`, `record_feedback`, refined `search_memories`. Shape + envelope conformance. |
| `chitta::synthesis` decision logic | Medium | Threshold tests with synthetic clusters (size <5, exactly 5, mixed-day, all-old); contradiction-detection happy + edge paths; mocked LLM extraction interface |
| Integration (DB-backed) | Medium | New schema integration tests in `tests/integration.rs` style: full /reflect cycle on a fixture, /agree/disagree round-trip with confidence deltas, supersession flow, idempotency on the new schema |
| `chitta-migrate` | Low | One end-to-end smoke test on a tiny fixture old-DB. Throwaway code; not worth deep test investment. |

### Two separate yojana deliverables (out of this PRD)

- **Gold set authoring task.** ~50 hand-authored entries `{context, query?, expected_memory_ids, rationale}`. Authored from existing observations. Plugged into astrobench. Targets recall@5, recall@10, MRR per tier. Drives v0 number tuning (half-life, /reflect threshold, /agree/disagree increments).
- **Replay harness task.** 10–20 past transcripts. Each session has hand-identified preference-relevant moments. At each moment, the harness snapshots the context that *would* have been available and runs the pipeline as-of that moment's DB state. Tests whether the surrounding context is rich enough to drive tier 1; gold set tests retrieval mechanics.

### Online metrics (passive, post-launch)

Tracked but not used as optimization targets:

- Profile hit rate — fraction of session responses that reference a tier-0 memory.
- Tier 1 fill rate — fraction of context-faceted queries returning ≥1 result.
- Reinforcement velocity — fraction of returned memories that get a `/agree` or `/disagree` event within the session.
- Supersession events from /reflect.

These signal degradation or pattern shifts; they don't validate design.

## Phased delivery

| Phase | Scope | Sizing |
|---|---|---|
| 1 | Schema + rename | ~2 days. One migration `0001_init.sql`. Crate / binary / DB renamed. Old config kept available for the seed step. |
| 2 | Tool surface port | ~2 days. Existing 7 tools updated for new schema; new `get_profile`, `supersede_memory`, `record_feedback`. `search_memories` refined. |
| 3 | Migration tooling + seed | ~1 day for tooling + a couple hours of focused hand-editing. ~30 rows imported at confidence=0.70, tagged `seed:2026-05`. Old DB renamed and locked read-only. |
| 4 | Gold set authoring (separate deliverable, separate task) | ~1–2 days |
| 5 | Astrobench wiring | ~1 day |
| 6 | First measurement + tune | open-ended; tune half-life, thresholds, increments based on gold-set scores |
| 7a | Caller alignment: `/done` rewrite + `~/CLAUDE.md` update + manual `/reflect` skill scaffold | ~1 day. **Critical-path with the schema landing** — without this, `/done` writes break against the new validators. |
| 7b | `/reflect` rewrite (the substantive synthesis pipeline) | ~1 week |
| 8 | `/agree`, `/disagree` skills | ~2 days |

Phases 1–3 + 7a are the lockstep release that lands the schema break. Phases 4–6 unlock the measurement loop. Phase 7b is the hard part where prediction is least reliable. Phase 8 closes the feedback loop.

## Out of Scope

- **Vidya design.** A peer subsystem for knowledge graphs is planned. Domain-knowledge rows in old chitta are left for vidya to mine when it starts. Separate PRD.
- **Importing old project-artifact decisions into yojana.** May never happen; the archive remains accessible if it does. Out of this PRD's scope.
- **HyDE, reranking, query rewriting.** Mature-stack additions. No signal yet that they help working-model retrieval. Defer until measurement.
- **Auto-triggered /reflect.** v0 is manual only. Auto-triggering is a measurement-driven decision.
- **Multi-subject schema.** No `subject` column. If a second subject ever surfaces, it's a one-line ALTER TABLE plus new code paths — not v0.
- **Modelling the agent or the collaboration as a working-model subject.** Parked observation; revisit if patterns surface that warrant it.
- **Implicit-from-behavior reinforcement and LLM self-report reinforcement.** Direction doc rejected both for v0. `/agree` `/disagree` are the only feedback surface.
- **/reflect prompt design at word level.** The pipeline shape and threshold logic are PRD-scope; the LLM prompts are iterated in implementation.
- **Per-type confidence/threshold tuning.** Single global rule in v0; per-type comes after measurement.

## Further Notes

- Principle alignment: this PRD strengthens principles 1 (verbatim sacred), 2 (bi-temporal), 3 (write fast / enrich lazily — synthesis is /reflect's job, not the write path), 4 (agent-native envelope, all new tools conform), 5 (small core: 10 tools), 6 (idempotency preserved), 7 (single-profile design preserves the multi-tenant story), 8 (every new validator returns actionable errors), 9 (no write-time extraction — synthesis stays out of the write path), 10 (no new dependencies introduced), 11 (the human owns the data — the seed step's hand-edit is the human-curation moment).
- Principle 11 deserves a callout: the PRD's seed step is *the* moment of human ownership. Josh hand-picks what defines the working model. That's the right cost.
- The 180-day decay half-life, the N≥5 /reflect threshold, the 0.05/0.10 agree/disagree increments, and the cluster-size-confidence formula are **all v0 starting values**, expected to be tuned. Gold-set evaluation drives the first round of tuning.
- The `seed:2026-05` tag is a useful cohort marker — gold-set scoring can compare "performance on seeded vs /reflect-emitted" rows once /reflect produces non-trivial output.
- Acceptance: Josh approves; PRD ready to drive implementation tasks decomposed via `vidhi-decompose`.
