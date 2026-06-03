---
name: agree
description: "Reinforce consolidated memories. Bumps confidence +0.05 per memory. Use when a working-model entry rings true."
---

# Agree — Reinforce Working-Model Memories

**Execute this workflow. Do not just describe it.**

## Input

`$ARGUMENTS` contains zero or more memory IDs (UUIDs or prefixes).

## Steps

### Step 1: Resolve memory IDs

If `$ARGUMENTS` contains explicit IDs, use those.

If `$ARGUMENTS` is empty or descriptive (e.g., "the coding style one"), identify
the relevant consolidated memory IDs from conversation context — prior
`get_profile` results, `search_memories` results, or `get_memory` calls made
during this session.

**Do not guess.** If you cannot confidently identify which memories to agree
with, ask the user to specify. There is no "last" shorthand — you must supply
concrete UUIDs.

### Step 2: Call record_feedback for each

For each resolved memory ID:

```
record_feedback(
  profile: "josh",
  memory_id: "<uuid>",
  kind: "agree"
)
```

### Step 3: Report results

For each call, report:
- Memory ID (short prefix is fine)
- Content snippet (first ~80 chars)
- New confidence value
- Any errors (e.g., memory not found, not a consolidated type)
