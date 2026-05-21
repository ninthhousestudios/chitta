---
name: reflect
description: "Consolidate observations into mental models. Synthesize raw session material into lasting knowledge."
---

# Reflect — Interactive Working-Model Synthesis

**Execute this workflow. Do not just describe it.**

You are the synthesis engine. Read raw observations, spot patterns, propose
consolidations, and let Josh approve or reject each one.

## Step 1: Gather raw material

Call `reflect_status(profile: "josh")` to see what's available since the last
status run. Display a brief summary:

- Period covered (since → now)
- Row counts by type
- Any disagree-flagged memories

If there are 0 rows, say so and stop.

## Step 2: Read the raw rows

Call `search_memories` with `memory_types: ["observation", "episode", "decision"]`
and a broad query (or `list_recent_memories`) to fetch the actual content of
the raw rows from the period. You need to see the content to do synthesis.

Also call `search_memories` with `memory_types: ["trait", "value", "pattern", "preference", "mental_model"]`
to fetch existing consolidated memories so you can detect contradictions and
avoid duplicates.

## Step 3: Propose consolidations

Read through the raw material. Look for:

- **Recurring themes** — the same preference/value/pattern appearing across
  multiple observations or sessions
- **Contradictions** — new observations that conflict with existing consolidated
  memories (these become supersessions)
- **Novel signals** — traits/values/patterns not yet captured in the working model

For each proposed consolidation, present it clearly:

```
### Proposed: [type] — [claim]

**Claim:** [the consolidated statement]
**Type:** trait | value | pattern | preference | mental_model
**Based on:** [list the source observations, with snippets]
**Confidence:** [0.50-0.90, based on strength of evidence]
**Contradicts:** [existing memory ID and content, if applicable]
```

Present ALL proposals at once, numbered, then ask Josh to approve/reject/edit
each one. Use AskUserQuestion with multiSelect so he can approve a batch.

## Step 4: Write approved consolidations

For each approved consolidation:

1. Call `store_memory` with:
   - `memory_type`: the consolidated type (trait/value/pattern/preference/mental_model)
   - `content`: the approved claim text
   - `profile`: "josh"
   - `tags`: ["reflect", "synthesised"]
   - `source`: "reflect"
   - `confidence`: the proposed confidence value

2. If it supersedes an existing memory, call `supersede_memory` with the old
   memory's ID and the new memory's ID.

## Step 5: Summary

Report what was written: how many consolidations stored, any supersessions,
how many proposals were rejected.

Do NOT write a reflect_runs marker — the reflect_status call in Step 1
already wrote a status-type run marker. The synthesis watermark (used by the
disabled CLI pipeline) is intentionally left alone.

## Important

- Never write a consolidation without explicit approval
- If Josh edits a claim, use his wording exactly
- Keep claims concise — one clear sentence
- Prefer updating/superseding existing consolidated memories over creating
  near-duplicates
- If there are very few raw rows (< 5), it's fine to propose nothing — say
  "not enough material yet" and stop after the summary
