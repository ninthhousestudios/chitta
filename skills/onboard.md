---
name: onboard
description: "Structured Q&A to seed or enrich a Chitta working model. Use when onboarding a new profile or when the profile is thin and needs intentional seeding."
---

# Onboard — Working Model Q&A

**Execute this workflow. Do not just describe it.**

## Overview

Run a structured Q&A interview to seed high-quality observations in Chitta,
then consolidate them into typed memories (trait, value, pattern, preference,
mental_model). Produces cleaner material than passive session observations.

## Prerequisites

- Chitta MCP connected and healthy
- Profile name known (default: "josh")

## Workflow

### Step 1: Check current profile

Call `get_profile` and review what's already there. Note gaps — don't
re-ask what's already well-covered.

### Step 2: Run Q&A rounds

Ask 3-4 questions per round across these facets:

| Facet | Example questions |
|-------|-------------------|
| **Values** | What matters most in software design? What tradeoffs do you optimize for? |
| **Workflow** | How do you learn? How do you make decisions? What's your work rhythm? |
| **Background** | What did you do before this? What shaped how you think? |
| **Collaboration** | How should Claude disagree? What frustrates you about AI? |
| **Preferences** | Language/tool preferences? Tolerance for "good enough"? |
| **Aspirations** | Where are you trying to get to? What does "done" look like? |

Guidelines:
- **Adapt questions based on answers.** Follow interesting threads.
- **Don't ask what the profile already covers.** Check Step 1.
- **Short answers are fine.** Don't pressure for depth.
- **3-5 rounds is typical.** Read energy — stop when answers thin out.
- **Ask about the person, not the project.** Project facts belong in yojana.

### Step 3: Store observations

After each round, store observations in Chitta:

```
store_memory(
  profile: "<profile>",
  memory_type: "observation",
  content: "<1-3 sentence distillation of what was said>",
  tags: ["qa-session", <topical tags>],
  applies_to_situations: [<relevant situations>],
  idempotency_key: "onboard-<facet>-<short-slug>",
  source: "claude-code"
)
```

Capture what was said accurately. Don't editorialize or infer beyond
what was stated. One observation per distinct point — don't merge
unrelated answers.

### Step 4: Consolidate

After Q&A is complete, synthesize observations into consolidated types:

| Type | Use for |
|------|---------|
| `trait` | Enduring characteristics (breadth-over-depth, intuition-driven) |
| `value` | What matters to them (correctness, efficiency-as-design) |
| `pattern` | Recurring behaviors (1-3hr sessions, learns-by-doing) |
| `preference` | What they prefer/like/dislike (Rust, anti-Java) |
| `mental_model` | How they think about something (collaborative cognition) |

For each consolidated entry:
- Set `confidence` to 0.7-0.9 based on how clearly it came through
- Include `derivations` linking to source observation IDs when possible
- Tag with `["qa-session", "consolidated"]`
- Keep content to 2-4 sentences

### Step 5: Report

Show the user a summary table of what was consolidated:
type, short content description, confidence. Ask if anything
needs correction before ending.

## Handling Chitta drops

If Chitta's MCP session drops mid-workflow (common with long sessions):
- Tell the user to run `/mcp` to reconnect
- Retry the failed stores — idempotency keys prevent duplicates
- Don't re-ask questions; you have the answers in context

## What NOT to store

- Project-artifact facts (route to yojana)
- Things already in docs or code
- Domain knowledge (future: vidya)
- Trivial or obvious statements
