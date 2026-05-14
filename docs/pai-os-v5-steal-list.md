# PAI-OS v5.0.0 — Steal List for Chitta

Source: `~/soft/pai-os/Releases/v5.0.0/`
Date: 2026-05-13

PAI-OS is a "Life Operating System" built on Claude Code — hooks, skills, agents,
and a file-based memory system. v5.0.0 ships 37 hooks, 45 skills, 171 workflows,
a named Digital Assistant persona, and a 7-phase execution engine ("the Algorithm")
that frames every task as current-state → ideal-state via verifiable iteration.

This doc extracts what's relevant to chitta from PAI's memory, relationship,
satisfaction, and user-modeling subsystems.

---

## High-Value Ideas

### 1. Satisfaction Signal as First-Class Data

PAI runs `SatisfactionCapture.hook.ts` on every `UserPromptSubmit`. Every single
user message produces a 1–10 rating via three paths (priority order):

1. **Explicit** — bare number or word form ("eight"), regex-parsed with false-positive
   rejection ("10 items" doesn't match).
2. **Praise fast-path** — 20 positive words + 12 phrases → rating 8, confidence 0.95,
   skip inference.
3. **LLM inference** — sends last AI response + 4 turns of context to a classifier.
   Rating scale is specified per-level (e.g., "terse redirects = 3-4",
   "repeated requests = 2-3").

Key design decision: **neutral = 5, not null.** A previous version returned null for
neutral prompts, creating survivorship bias. Now every non-system prompt gets a rating.

Stored as JSONL: `{ timestamp, rating, session_id, source, sentiment_summary,
confidence, response_preview, comment? }`. The `response_preview` (first 500 chars
of last AI response) ties ratings to specific responses, not just sessions.

Low-rating actions:
- Rating < 5 → categorized learning file (SYSTEM or ALGORITHM category)
- Rating ≤ 3 → full context dump: transcript, sentiment analysis, tool calls,
  8-word LLM-generated description

**Chitta relevance:** Chitta has `record_feedback` but it's not systematic. A
per-session satisfaction signal would ground `/reflect` in actual data, enable
confidence adjustments on observations that correlate with low/high ratings, and
surface anti-patterns. Could be a PostToolUse hook on session end or a done-skill
integration.

### 2. Declared vs Inferred Provenance

PAI separates user knowledge into two layers:

- **USER/** — declared identity, user-curated, loaded at every session start. Includes
  PRINCIPAL_IDENTITY, TELOS (mission/goals/beliefs/wisdom), OPINIONS, WRITINGSTYLE,
  TECHSTACKPREFERENCES, etc. Static within sessions.
- **RELATIONSHIP/** — learned signals, accumulated automatically from transcript
  analysis. Daily markdown files with typed entries. Never mutates USER/.

Chitta currently mixes these: a consolidated `preference` from `/reflect` and a raw
`observation` from a session live in the same pool with the same retrieval path.
There's no way to know whether a memory represents something Josh explicitly stated
vs something inferred from behavior.

**Possible implementation:** Add an optional `source_type` field to memories:
`declared`, `inferred`, `consolidated`. Profile loader could weight declared higher
or present them in grouped sections. Doesn't require schema migration if it's just
a metadata field.

### 3. Negative Knowledge / Anti-Patterns

PAI has a `LEARNING/` subsystem with:
- `ALGORITHM/` — task execution failures (wrong approach, over-engineered, missed the point)
- `FAILURES/` — full-context dumps for ratings 1–3
- `SYNTHESIS/` — periodic pattern reports aggregating across failures
- `REFLECTIONS/` — per-session algorithm performance self-assessment

Chitta stores observations but doesn't structurally distinguish "Josh prefers X" from
"X approach failed and Josh corrected it." Corrections and frustrations are arguably
the most durable working-model signal — people's negative preferences are more stable
than their positive ones.

**Possible implementation:** A `polarity` field (`positive` | `negative` | `neutral`)
or a dedicated `memory_type: "anti-pattern"`. Profile could surface these as
guardrails alongside preferences. `/reflect` could consolidate negative observations
into anti-patterns the way it consolidates positive ones into preferences/patterns.

### 4. Typed Relationship Notes (W/B/O)

PAI's `RelationshipMemory.hook.ts` fires at session end and writes daily logs with
typed entries:
- `W @Entity:` — world/objective facts about the principal's situation
- `B @DA:` — what the assistant did this session (first-person)
- `O(c=0.85) @Entity:` — opinion/preference with explicit confidence at creation

Design details:
- Positive signals require ≥2 occurrences before generating a note (confidence 0.70)
- Frustration signals require ≥2 occurrences (confidence 0.75)
- Uses regex pattern detection, not LLM analysis (acknowledged as a limitation)

**Chitta relevance:** The confidence-at-creation based on signal strength is interesting.
Chitta currently starts observations at default confidence and adjusts via
reinforcement. PAI front-loads the confidence estimate based on how strong the
evidence was. Could inform how `store_memory` sets initial confidence — e.g., if the
observation is from a correction (high signal), start at 0.8; if from ambient
behavior, start at 0.5.

### 5. Temporal Validity on Memories

PAI's knowledge entities have `valid_from`/`valid_until` fields. Their contradiction
detector uses these to correctly handle facts that were true at different times.

Chitta has bi-temporal tracking (event_time + record_time) but no validity window.
A preference Josh had in 2025 might not hold in 2026 — the only option is
`supersede_memory`, which marks the old one as superseded but doesn't express
"this was true from X to Y."

**Possible implementation:** Optional `valid_until` on memories. Profile loader
filters by current validity. Supersede could auto-set `valid_until` on the old entry.
Low priority — decay-weighted effective_score already handles staleness somewhat.

### 6. Automated Pattern Synthesis

PAI runs `LearningPatternSynthesis.ts` to generate weekly/monthly aggregation reports
from the raw signal stream (ratings, relationship notes, learning entries). This is
automated and periodic.

Chitta has `/reflect` which consolidates observations manually within a session. The
gap: no scheduled cross-session synthesis.

**Possible implementation:** A cron routine that runs `/reflect` periodically against
recent observations. Could use the `schedule` skill or a systemd timer. Already
partially covered by the plan for scheduled routines in manas.

---

## Interesting but NOT Chitta's Job

- **TELOS (mission/goals/current-state/ideal-state)** — life planning, not user
  modeling. Closer to a yojana concern or a separate manas service.
- **ISA (Ideal State Artifact)** — per-task specification with 12 sections and
  verifiable criteria. Already covered by yojana's task graph.
- **The Algorithm (7-phase engine)** — workflow orchestration. Manas equivalent is
  the skill/hook system.
- **Knowledge graph (People/Companies/Ideas/Research)** — domain knowledge with
  entity types, wikilinks, BM25 search. Planned for vidya.
- **Mode/effort classification** — Sonnet-backed prompt classifier that decides
  MINIMAL/NATIVE/ALGORITHM and effort tier E1–E5. Interesting but orthogonal to
  working-model storage.

---

## PAI Memory Architecture (Reference)

Storage: flat markdown + YAML frontmatter. No database, no vector store. Retrieval
via BM25 (`MemoryRetriever.ts`) and in-memory graph (`KnowledgeGraph.ts`).

Directory structure under `~/.claude/PAI/MEMORY/`:
```
WORK/          — per-task ISA.md files (current-state → ideal-state)
KNOWLEDGE/     — curated entity graph (People, Companies, Ideas, Research)
LEARNING/      — failures, reflections, pattern synthesis
RELATIONSHIP/  — daily typed notes (W/B/O) from transcript analysis
WISDOM/        — extracted atomic wisdom artifacts
RESEARCH/      — full agent output archives
STATE/         — ephemeral runtime (session state, events.jsonl)
USER/          — declared identity (loaded every session)
```

Knowledge entity lifecycle: `seedling → budding → evergreen`
Lookup test: "Would the user look this up by name?" Yes → KNOWLEDGE. No → WORK or LEARNING.

Satisfaction ratings: JSONL in `LEARNING/SIGNALS/ratings.jsonl`, one JSON object per
line, never rewritten.
