---
name: disagree
description: "Push back on consolidated memories. Drops confidence -0.10 per memory, with optional correction text for /reflect to pick up."
---

# Disagree — Challenge Working-Model Memories

**Execute this workflow. Do not just describe it.**

## Input

`$ARGUMENTS` contains one or more memory IDs, optionally followed by `--`
and a correction string.

Formats:
- `/disagree <id>` — disagree, no correction
- `/disagree <id1> <id2>` — disagree with multiple
- `/disagree <id> -- <correction text>` — disagree with correction

The correction applies to all listed IDs.

## Steps

### Step 1: Parse arguments

Split `$ARGUMENTS` on `--`. Left side: memory IDs (space-separated UUIDs or
prefixes). Right side (if present): correction text.

If `$ARGUMENTS` is empty or descriptive, identify the relevant consolidated
memory IDs from conversation context — prior `get_profile` results,
`search_memories` results, or `get_memory` calls made during this session.

**Do not guess.** If you cannot confidently identify which memories to disagree
with, ask the user to specify. There is no "last" shorthand — you must supply
concrete UUIDs.

### Step 2: Call record_feedback for each

For each resolved memory ID:

```
record_feedback(
  profile: "josh",
  memory_id: "<uuid>",
  kind: "disagree",
  correction: "<correction text or omit if none>"
)
```

When correction is provided, `record_feedback` writes a separate observation
tagged `contradicts:<memory_id>` that `/reflect` picks up as contradicting
evidence during synthesis.

### Step 3: Report results

For each call, report:
- Memory ID (short prefix is fine)
- Content snippet (first ~80 chars)
- New confidence value
- Whether a correction observation was created
- Any errors (e.g., memory not found, not a consolidated type)
