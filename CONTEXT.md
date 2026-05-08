# chitta

chitta is the **working model of Josh** — a stored, evolving model of what he values, how he works, what he prefers, and what mental models he uses, available to every agent in manas across every domain.

The Sanskrit *citta* is the field of impressions (samskāra) that conditions future thought and behavior; that is precisely what this subsystem holds.

## Language

**Working model**:
chitta's contract — the model of Josh that an agent loads to act consistently with him.
_Avoid_: personality, personality store, personhood store, identity store, persona

**Josh model**:
Informal shorthand for the working model. Acceptable in conversation; prefer "working model" in docs.

**Profile**:
chitta's only isolation primitive (principle 7). Required argument on every tool. v0 has one profile: `josh`. New profiles are only created when the *subject* changes.
_Avoid_: workspace, tenant, namespace, room

**Subject**:
Who a memory is about. v0 has one subject: Josh. There is no `subject` column in v0 schema — every row is implicitly about Josh. Adding a second subject is a future migration (one ALTER TABLE). Modelling the agent or the collaboration is parked, not on a roadmap.

**Facet** (applies_to_*):
A column on a memory that scopes its retrieval relevance. Four canonical facets in v0, each `text[]`:
- `applies_to_domains` — subject areas (e.g. `rust`, `astrology`, `writing`, `architecture`).
- `applies_to_skills` — claude-code skills the memory speaks to (e.g. `review`, `done`, `init`).
- `applies_to_projects` — project slugs (e.g. `chitta`, `yojana`).
- `applies_to_situations` — state-of-being or use-context shapes (e.g. `decision-making`, `code-review`, `naming`, `tired`, `late-night`).
Vocabularies inside each facet are **convention-driven, not schema-locked** in v0 — tuned by gold-set authoring.
Adding a fifth facet requires a migration (deliberate friction).

**Memory**:
A row in chitta. Bi-temporal (`event_time`, `record_time`), tagged by `memory_type`, scoped to one profile. A row exists in chitta only if it contributes to the working model; bare project artifacts belong elsewhere.
_Avoid_: entry, record, fact

**Episode** (memory_type):
A time-bounded recounting that aggregates raw observations from a session, task, or interaction. Distinct from `observation` (a single noticing) by being aggregative — an episode points back at the observations it covers. Written by the **harness or skills** at the end of a unit of work (e.g. the `done` skill at session end). **Never written by /reflect** — /reflect reads episodes alongside observations and emits consolidated rows from them.

A chitta `episode` row has a hard-validated shape:

- ≥ 1 entry in the `derivations` table linking the episode to a source memory, written atomically with the episode itself.

Soft conventions (not validated): `external_refs` carries a session pointer when applicable; `applies_to_situations` carries at least one situation label; `event_time` is the end of the period the episode covers.

If derivations are missing, `store_memory(memory_type=episode, ...)` is rejected with: *"episode memory requires at least one entry in derivations linking to source observations. Either supply derivations, or use memory_type=observation."*

**Decision** (memory_type):
A recorded choice stored in chitta **only when the choice reveals working-model signal** — values, mental models, or decision style. The choice itself (what was picked, on which project) is incidental; the rationale and rejected alternatives are what /reflect mines. Project-artifact decisions belong in **yojana**, not here.

A chitta `decision` row has a **hard-validated shape**:

- `metadata.rationale: string` — required, non-empty.
- `metadata.rejected_alternatives: string[]` — required, length ≥ 1.

Without both, `store_memory(memory_type=decision, ...)` is rejected with an instruction (per principle 8): supply the missing fields, demote to `observation`, or route to yojana.

**Layer**:
chitta content is partitioned into three layers by `memory_type`:
- **Raw / episodic** — `observation`, `episode`, `decision`. Append-only, immutable.
- **Consolidated / semantic** — `trait`, `value`, `pattern`, `preference`, `mental_model`. Confidence-weighted, supersedeable.
- **Profile / always-on** — derived view over the consolidated layer (top-N). Cached.

**Tier**:
A retrieval pipeline. Tier 0 = `get_profile` (no query). Tier 1 = context-faceted SQL (no embedding). Tier 2 = hybrid dense+sparse RRF.
_Avoid_: using "tier" to mean "layer" — they are orthogonal.

**Synthesis**:
The /reflect job that reads raw-layer memories and emits consolidated-layer memories, with `derivations` rows pointing back at sources.

**Supersession**:
A consolidated memory is replaced by a newer one when /reflect detects contradiction. The old row is not deleted; `superseded_by` is set, and default reads filter it out.

**Reinforcement**:
Bumping `reinforcement_count` and `last_reinforced_at` on a consolidated memory. Triggered by `/agree` (and, later, by implicit signals once the harness can detect them).

**Confidence**:
A 0–1 score on consolidated memories. The stored value is mutated by reinforcement (`/agree`) and disagreement (`/disagree`); the audit trail of those mutations lives in raw-layer feedback rows. Null on raw-layer rows.

**Effective score**:
Computed at read time in app code (not SQL): `confidence * decay(last_reinforced_at, now)`. Used to rank tier-0 contents. The decay function and any score composition live in **one app-side module** so the formula evolves in one place; SQL returns a candidate set ordered by raw confidence, app code refines with decay and truncates.

**Derivation**:
A typed link from a consolidated memory to a raw-layer memory it was synthesized from. Lineage; lets a reader explain "why does chitta think this about Josh?"

## Relationships

- A **Working model** is composed of **Memories** across three **Layers**.
- Every **Memory** belongs to exactly one **Profile**.
- **Synthesis** reads **Raw layer** memories and writes **Consolidated layer** memories, plus **Derivations** linking them.
- **Supersession** links one **Consolidated** memory to another it replaces.
- The **Profile layer** is computed from the **Consolidated layer**, never authored directly.

## Boundaries (what is *not* chitta)

- **Project / task memory** lives in **yojana** — including project-artifact decisions ("we picked Postgres for chitta," "we shipped feature X"). A decision lands in chitta *only* when it carries working-model signal (rationale + rejected alternatives that reveal Josh's values, patterns, or mental models). Routing is the caller's job; chitta does not deduplicate against yojana.
- **Knowledge graphs** (vedic astrology entities, generic concept knowledge) live in **vidya** (a planned subsystem, peer to chitta).
- **Document content** lives in **kosha** / **smriti**.
- **Code symbols** live in **sutra**.
- **Session transcripts** live on disk under `~/.sessions/<harness>/`.

If a piece of content fits one of these other subsystems, it does not belong in chitta — even if it could be embedded.

## Example dialogue

> **Josh:** "Store that I prefer deletion over feature flags."
> **Agent:** "That's a **preference** in the **consolidated layer**. I'll write it under profile `josh`. Do you want me to also link the **observations** that led to this — the discussion we had on the chitta refactor?"
> **Josh:** "Yes, link them as **derivations**."
> **Agent:** "Got it. If a future session contradicts this, /reflect will mark it **superseded** rather than overwriting."

## Flagged ambiguities

- "memory" was historically used for both a single row and the whole subsystem — resolved: a row is a **Memory**, the subsystem is **chitta** (or **the working model**).
- "profile" was used for both the always-on retrieval slice and the multi-tenant namespace — resolved: the retrieval slice is the **Profile layer** (always plural-of-content); the namespace is a **Profile** (capital P, singular). When in doubt, say "profile layer" or "profile namespace."
