---
name: reflect
description: "Consolidate observations into mental models. Synthesize raw session material into lasting knowledge."
---

<reflect_workflow>
**Execute this workflow. Do not just describe it.** You are the synthesis engine: read raw observations, spot patterns, propose consolidations, let Josh approve or reject each.

<step n="1" name="gather">
Call `reflect_status(profile:"josh")` for what's available since the last status run. Summarize briefly: period covered (since → now), row counts by type, any disagree-flagged memories. If 0 rows, say so and stop.
</step>

<step n="2" name="read_raw">
Call `search_memories` with `memory_types:["observation","episode","decision"]` and a broad query (or `list_recent_memories`) to fetch the actual content of the period's raw rows — you need the content to synthesize. Also call `search_memories` with `memory_types:["trait","value","pattern","preference","mental_model"]` to fetch existing consolidated memories, so you can detect contradictions and avoid duplicates.
</step>

<step n="3" name="propose">
Read the raw material for: recurring themes (same preference/value/pattern across multiple observations or sessions), contradictions (new observations conflicting with existing consolidated memories → supersessions), and novel signals (not yet captured). Present each proposal clearly:

```
### Proposed: [type] — [claim]
Claim: [consolidated statement]
Type: trait | value | pattern | preference | mental_model
Based on: [source observations, with snippets]
Confidence: [0.50-0.90, by evidence strength]
Contradicts: [existing memory ID + content, if applicable]
```

Present ALL proposals at once, numbered, then ask Josh to approve/reject/edit each. Use AskUserQuestion with multiSelect for batch approval.
</step>

<step n="4" name="write">
For each approved consolidation, call `store_memory` with: `memory_type` (the consolidated type), `content` (approved claim), `profile:"josh"`, `tags:["reflect","synthesised"]`, `source:"reflect"`, `confidence` (proposed value). If it supersedes an existing memory, call `supersede_memory` with the old and new memory IDs.
</step>

<step n="5" name="summary">
Report: how many consolidations stored, any supersessions, how many proposals rejected. Do NOT write a reflect_runs marker — the Step 1 `reflect_status` call already wrote a status-type run marker. Leave the synthesis watermark (used by the disabled CLI pipeline) alone.
</step>

<rules>
- Never write a consolidation without explicit approval.
- If Josh edits a claim, use his wording exactly.
- Keep claims concise — one clear sentence.
- Prefer updating/superseding existing consolidated memories over near-duplicates.
- If very few raw rows (<5), it's fine to propose nothing — say "not enough material yet" and stop after the summary.
</rules>
</reflect_workflow>
