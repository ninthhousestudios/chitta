# chitta — personality pivot

Status: design direction (pre-PRD)
Date: 2026-05-07
Decision record: chitta memory `019e0453-ad7d-7912-92bb-e36101158f55`

This doc captures the direction we settled on in the 2026-05-07 design discussion. It is the input to the actual PRD (yojana task to follow). Treat the contents as decided-direction, not yet committed implementation.

---

## the pivot

Chitta is the origin of manas. In its original shape it tried to be everything — project memory, knowledge graph, learning, multi-tenant, 38 tools. The chitta-rs rewrite trimmed it to a verbatim, bi-temporal, write-fast/enrich-lazy core. But it still hosts at least three different shapes of content under one contract:

1. **Project / work artifacts** — decisions, session summaries about a codebase. Yojana now owns this slice.
2. **Domain knowledge** — vedic astrology entities, generic concept entries. Cognee-territory.
3. **Josh-patterns** — preferences, values, pushbacks, decision style.

Three different shapes living under "memory" is the same kitchen-sink problem in smaller clothing. We're cutting it.

**New contract:**

> Chitta is the model of Josh — patterns, preferences, values, style, how he thinks.

Project memory belongs to yojana. Knowledge belongs to a new subsystem (vidya — see below). Chitta narrows to personality/identity.

The Sanskrit name चित्त (consciousness/mind) actually fits this framing better than "memory bank."

### why personality is the right narrowing

- **It's the actual gap.** Yojana = tasks, sutra = code, smriti = files, kosha = documents. Nothing in manas captures *who Josh is*.
- **It's hardest to outsource.** Cognee, Mem0, Letta, LangMem all do knowledge graphs. Nobody does "stable, evolving model of the human I work with" particularly well.
- **It travels across domains.** The Josh model applies whether you're coding, doing astrology, writing, or thinking about life. Knowledge stores are typically domain-bound.

### vidya — the parallel direction

A new subsystem, peer to chitta. Sits on top of kosha/sutra/smriti and ingests via cross-LLM extraction (different models summarizing the same documents — built-in benchmarking story). Holds knowledge graphs. Domain entities currently in chitta migrate there when ready. Out of scope for the chitta PRD; just don't paint chitta into a corner.

---

## binary / db rename

As part of this pivot we collapse `chitta-rs` back to `chitta` for naming consistency with the rest of manas (no other subsystem uses a `-rs` suffix even though they're all Rust):

- Crate name: `chitta-rs` → `chitta`
- Binary: `chitta` (already)
- Default DB name: `chitta_rs` → `chitta`
- Test DB name: `chitta_rs_test` → `chitta_test`
- `CHITTA_HOME` (already correct), env vars unchanged
- README, principles doc, manas-architecture references updated

This is mechanical but should land in the same migration as the schema break — one clean cut.

---

## memory taxonomy — three layers

| Layer | Memory types | Lifecycle | Volume |
|---|---|---|---|
| **Raw / episodic** | `observation`, `episode`, `decision` | append-only, immutable | thousands → tens of thousands |
| **Consolidated / semantic** | `trait`, `value`, `pattern`, `preference`, `mental_model` | superseded never deleted; confidence-weighted | tens → low hundreds |
| **Profile / always-on** | derived view over consolidated, top-N | computed/cached | ~20–50 |

Why three layers and not two: the always-on profile is what makes personality actually usable — it's the slice that loads every session without a query. Without it we're back to "did the agent remember to search."

---

## schema (clean break)

Fresh DB, new schema, no in-place migration. Old DB stays for archival / vidya migration material.

### dropped

- FTS column / index — astrobench showed it adds nothing on top of dense+sparse.
- Generic `memory` type — every row gets a real type.
- Most current rows. Hand-pick the high-signal ones to seed the new DB.
- `session_summary` as a freeform blob — replaced with `episode` that requires `derivations`.

### kept (load-bearing)

- Bi-temporal (`event_time`, `record_time`) — principle-level, non-negotiable.
- Profile scoping, idempotency key.
- Dense (1024-dim BGE-M3) + sparse vectors with RRF.
- `derivations` table — but actually populated this time.
- `external_refs` typed shape.
- `query_log` — for replay/regression eval.

### new

| Column / table | Purpose |
|---|---|
| `memory_type` enum: `observation`, `episode`, `decision`, `trait`, `value`, `pattern`, `preference`, `mental_model` | tight, no escape hatch |
| `superseded_by UUID NULLABLE` | first-class supersession |
| `confidence FLOAT NULLABLE` | null on raw; 0–1 on consolidated |
| `applies_to JSONB` | `{domains:[], skills:[], projects:[], situations:[]}` for tier-1 facet filter |
| `reinforcement_count INT`, `last_reinforced_at TIMESTAMPTZ` | for confidence updates and decay |
| `subject` enum: `self` (Josh), maybe later others | optional, makes intent legible |

### profile reorganization

Replace single `chitta` profile with:

- `josh` — the Josh model. Consolidated layer + episodic.
- `josh-work-<project>` — per-project episodic (decisions, session summaries scoped to a codebase). /reflect can roll personality signal up into `josh`.

Domain knowledge profiles do not move; they go to vidya when ready.

---

## retrieval pipeline (v0)

All three tiers live inside chitta. No cross-tier calls (principle 9 stands). The caller (agent, skill, harness) supplies context as plain values; chitta uses them as filters.

```
tier 0  get_profile(profile)
        SELECT * FROM memories
        WHERE memory_type IN ('trait','value','preference','pattern')
          AND superseded_by IS NULL
        ORDER BY (confidence * recency_decay(last_reinforced_at)) DESC
        LIMIT 30
        — cached in process, invalidated on consolidated-layer write

tier 1  search_memories(context={...}) without query string
        SELECT * FROM consolidated
        WHERE applies_to ⊇ context
          AND superseded_by IS NULL
        ORDER BY confidence DESC LIMIT k
        — pure SQL, no embedding call

tier 2  search_memories(query, context?)
        existing hybrid dense+sparse RRF
        FILTER superseded_by IS NULL by default
        FILTER memory_type IN consolidated by default;
        flag include_raw=true to recall episodes/decisions
        ORDER BY confidence × similarity
```

Default scope of tier 2 is the consolidated layer. Raw layer is opt-in via `include_raw` or a sibling `recall_episode(query)` tool. Reason: most calls want "what's Josh's pattern about Y," not "find the exact moment Josh said Y." When the latter is wanted, ask explicitly.

Deferred: HyDE, reranking, query rewriting. These are mature-stack additions; we have no signal yet for whether they help personality retrieval. Measure first.

---

## tool surface

Stay close to current 7. Two adds, one refinement:

- **add** `get_profile(profile)` — returns the always-on layer. No query. Cheap.
- **add** `supersede_memory(old_id, new_id, reason)` — first-class. Today's `delete_memory` stays for genuine mistakes; supersession is the normal case for evolving traits.
- **refine** `search_memories` — accept optional `context: {task, skill, domain, situation}` and `include_raw: bool`.

---

## lifecycle / governance

The hard part — and where current chitta is weakest.

- **Synthesis** (raw → consolidated). /reflect's job. Currently makes mental_models; will gain trait/pattern/value/preference outputs. Each new consolidated memory writes `derivations` rows pointing at the observations that fed it.
- **Supersession.** When /reflect detects contradiction, it writes a new consolidated memory with `superseded_by` set on the old one. Old isn't deleted, just stops appearing in default reads.
- **Reinforcement.** Explicit `/agree` and `/disagree` shortcuts (Josh in the loop). Implicit-from-behavior and LLM self-report rejected for v0 — implicit is too noisy without harness work, self-report risks hallucination. Layer in implicit later once we know what good signal looks like.
- **Decay.** Confidence multiplied by age-decay function at read time, not write time. No background pruning. Old traits silently fall below threshold and stop surfacing without losing their record.
- **Surfacing changes.** When a trait gets superseded, /reflect writes a session-summary-style observation about the change itself. That goes back into the raw layer and becomes part of the long arc.

### `/agree` and `/disagree` shapes

- Operate on the *last set of memory IDs returned by chitta in this session*, with optional explicit `<id>` form.
- `/agree`: bump `reinforcement_count` + `last_reinforced_at`. Cheap.
- `/disagree`: flag for /reflect to revisit; lowers effective confidence; does not supersede on its own.
- `/disagree <correction>`: same flag plus stores a contradicting observation as raw-layer material for /reflect.
- Both produce raw-layer rows tagged `feedback:agree` or `feedback:disagree` with `external_refs` to the memory in question.

---

## testing approach

There is no ground truth like there is for code retrieval. Two loops, one passive monitor.

### gold set (fast)

~50 hand-authored entries: `{context, query?, expected_memory_ids, rationale}`. Examples we can author from existing observations:

- `{skill:"review", file:"*.rs"}` → "Josh dislikes backwards-compat shims" + "Josh wants direct pushback over agreement"
- `{domain:"architecture", query:"should we add a feature flag?"}` → "Josh prefers deletion over feature flags"
- `{skill:"done"}` → "Josh wants session summaries to focus on outcomes not transcript content"

Score: recall@5, recall@10, MRR per tier. Plug into astrobench. Signal in days.

### session replay (slower, more realistic)

10–20 past transcripts. Identify "preference-relevant moments" by hand (~5–10 per session). At each moment, snapshot the context that *would* have been available (active task, file, skill). Run pipeline as of that moment's DB state. Did the right pattern surface?

Tests whether the surrounding context is rich enough to drive tier 1 — gold set tests retrieval mechanics, replay tests the system in motion.

### online metrics (passive)

Once running:

- profile hit rate — how often a tier-0 fact gets referenced in the response
- tier 1 fill rate — how often context-faceted returns ≥1 result
- reinforcement velocity — fraction of returned memories that get reinforced
- supersession events from /reflect

Don't optimize on these directly. They tell you when the system is degrading or when a pattern shifts; they don't tell you "is the design right."

### what each loop answers

| Question | Answered by |
|---|---|
| "Does tier 0 have the right facts?" | gold set |
| "Is `applies_to` faceting actually useful?" | gold set with/without context |
| "Does dense+sparse beat dense alone for personality queries?" | gold set, A/B retriever |
| "Is the consolidated layer rich enough?" | replay |
| "Is /reflect's synthesis aggressive enough?" | replay over time |
| "Is supersession working?" | injection test: insert contradicting observations, verify supersession after /reflect |

---

## phased plan

Ordered roughly. Steps 1–3 are mechanical; step 4 is the substantive one.

1. **Schema + rename** (~2 days). One migration, fresh DB. Crate/binary/DB renamed `chitta-rs` → `chitta`.
2. **Tool surface** (~2 days). Port store/get/search/get_profile/supersede. Drop the rest until they earn their place back.
3. **Seed** (~1 day). Hand-pick ~30 high-signal consolidated memories from old DB; write at confidence=0.7.
4. **Gold set** (~1–2 days). 50 entries authored from existing observations.
5. **Astrobench harness** (~1 day). Wire to gold set.
6. **First measurement.** Tier 0 vs tier 0+1+2. Adjust.
7. **/reflect rewrite** (~1 week). Synthesize trait/pattern/value/preference; emit supersessions on contradiction; populate derivations. The hard part.
8. **`/agree` `/disagree`** (~2 days). Skill or slash command surface. Raw-layer feedback rows.

Steps 1–6 unlock the loop. Step 7 is where measurement matters most and prediction is least reliable.

---

## open questions for the PRD

1. **Where does decision rationale live long-term?** Both project-specific (we picked X for this codebase) and personhood-trace (Josh decides like this). Direction: store in `josh-work-<project>`; /reflect extracts the personhood signal into `josh` as a pattern with derivations pointing back. PRD to confirm.
2. **/reflect synthesis aggressiveness.** Aggressive = traits emerge fast but lock in early. Conservative = traits emerge slowly but stay accurate. Direction: start conservative (N≥5 corroborating observations before emitting a trait); tune from gold-set + replay measurements.
3. **`get_profile` static or dynamic?** Static (refreshed by /reflect) vs dynamic (computed per session start). Direction: static, refresh on /reflect.
4. **HyDE: build now or measure first?** Direction: measure first via astrobench.
5. **Confidence formula.** Reinforcement count, recency, derivation count — exact weighting. Direction: pick a simple formula in the PRD, evaluate against gold set, tune.
6. **`/agree` `/disagree` surface.** Slash commands? Skills? Hooks? Affects how the harness picks up the signal. Direction: lightweight skill that calls a chitta tool (`reinforce_memory` / `flag_memory`).
7. **Old DB fate.** Archive in place? Migrate select rows during seed? Direction: archive in place; cherry-pick during seed step.

---

## references

- Decision record: chitta memory `019e0453-ad7d-7912-92bb-e36101158f55` (full rationale, rejected alternatives).
- Current principles: `docs/principles.md` (mostly compatible; sections on memory_type and retirement need revision in PRD).
- Current chitta-rs README: rename instructions land here.
- Manas architecture (chitta section): `../docs/manas-architecture.md` — update once PRD is approved.
- Cognee research: `docs/research/cognee-research.md` (informs vidya, not chitta).
- Cobanov memory essay: https://memory.cobanov.dev — generic-memory architecture; useful as a checklist for governance/lifecycle, not as a personality-specific design.
