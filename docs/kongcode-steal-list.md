# kongcode steal list

Ideas worth lifting from `~/soft/kongcode` after the chitta refactor lands.
Source files cited as `kongcode:<path>` for traceability.

## Context

kongcode runs three layered scoring systems on top of BGE-M3 dense retrieval:

1. **WMR** — fixed-weight 6-signal linear blend (cosine, recency, importance,
   access, neighbor-bonus, proven-utility, reflection-boost), with a small
   utility penalty. Always-on fallback.
   `kongcode:src/engine/graph-context.ts` (`scoreResults`, ~L424).
2. **ACAN** — "Attentive Cross-Attention Network." A tiny learned scorer:
   single-head dot-product attention between query and memory embeddings
   (`W_q`, `W_k` both 1024x64, scale `sqrt(64)`) concatenated with 6 auxiliary
   features and pushed through a learned linear head. Trained in a Node worker
   from a `retrieval_outcome` table once 5000+ labeled pairs accumulate.
   Hot-reloads weights via mtime check; cross-process file lock during training.
   `kongcode:src/engine/acan.ts`.
3. **Cross-encoder rerank** — bge-reranker-v2-m3 (606MB) via node-llama-cpp's
   `LlamaRankingContext.rankAll`. Two-stage: top-N=30 candidates from WMR/ACAN,
   rescored as (query, doc) pairs, blended 0.6 WMR / 0.4 cross, re-sorted.
   Cited as the config that hit 98.2% R@5 on LongMemEval.
   `kongcode:src/engine/graph-context.ts` L27-101.

The name "cross-attention" on the kongcode site refers to ACAN, not the
cross-encoder. ACAN is a learned bilinear scorer over embeddings plus a linear
head over auxiliary features — not a transformer cross-attention block. The
heavy lifting on retrieval quality is the cross-encoder, not ACAN.

## Worth stealing

### 1. Cross-encoder rerank stage (high value, low complexity)

Two-stage retrieve-then-rerank. After dense retrieval produces top-K (50 say),
send the top-N (30) (query, doc) pairs to bge-reranker-v2-m3, blend the
cross-encoder score with the existing score (0.6/0.4 is their default), re-sort
the top-N, and preserve the tail past N.

Why: chitta already runs BGE-M3 via ONNX, so the reranker pairs naturally
(same tokenizer family, same training lineage). Cross-encoder rerank is the
single biggest retrieval-quality lever for dense systems. Our own
`docs/post-v0.0.2-benchmark/personamem-improvements.md` and
`docs/research/innovation-potentials.md` already flag it.

Patterns worth copying verbatim:

- **Skip when `candidates <= 5`** — cheap retrievals don't need rerank, save
  the inference cost.
- **Truncate doc text to a fixed char budget** (kongcode uses 24000) before
  sending to the reranker context window.
- **Fail open** — `_rankingCtx = null` on init failure; rerank function returns
  candidates unchanged if model isn't loaded. Retrieval keeps working.
- **Preserve tail** — only the top-N gets rerank-shuffled; everything past N
  retains its original order. Avoids degrading recall for low-confidence hits
  and bounds inference cost.
- **Init at daemon startup, dispose on shutdown.** Single shared context.

Open question for chitta: ONNX vs llama.cpp for the reranker. We use ONNX for
the embedder; an ONNX cross-encoder keeps the runtime story consistent. llama.cpp
is what kongcode uses but adds a second runtime dependency.

### 2. Retrieval outcome logging (do this regardless)

Even if we never build ACAN, the `retrieval_outcome` table is independently
useful. Schema sketch:

```
retrieval_outcome:
  query_text
  query_embedding
  retrieved_id          // memory or concept id
  retrieved_table
  retrieval_score       // whatever the ranker emitted
  rank_position
  was_neighbor          // graph-expand flag (n/a for chitta yet)
  importance, access_count, recency  // snapshot of memory state at retrieval
  utilization           // downstream "did this help" signal
  llm_relevance         // optional LLM-graded relevance, overrides utilization
  created_at
```

Uses:
- offline analysis ("what queries miss?")
- regression tracking across releases / benchmark runs
- training corpus for any future learned scorer
- debugging: "show me the candidates considered for query X"

Cheap to add now, expensive to retrofit. The `utilization` signal is the hard
part — kongcode infers it from later memory access; we'd need to define what
"used" means in chitta's flow.

### 3. Hot-reload + cross-process locking pattern

kongcode runs multiple MCP processes against the same SurrealDB instance.
ACAN's solution is worth keeping in mind for any background-trained artifact
chitta accumulates:

- write to `path.tmp` then atomic `rename()` so partial writes are never
  visible (`kongcode:src/engine/acan.ts` `saveWeights`).
- `O_CREAT|O_EXCL` lockfile with a stale-lock-stealing rule (30 min) so a
  crashed trainer doesn't block forever.
- `mtime` check on the artifact file before each scoring call — if it's newer
  than what we loaded, hot-reload. Lets sibling processes pick up retrains
  without restart. Cost is one `statSync` per call.

We don't need this yet, but it's the right shape when we do.

## Skeptical of

### ACAN itself

- 140k learnable params trained on noisy "utilization" labels. The signal is
  weak: was a memory really useful, or did the agent just reference it?
- 5000-sample activation threshold means most users never trigger it. The
  fixed-weight WMR fallback does the actual work for them.
- With a real cross-encoder in the loop, a learned bilinear-plus-linear-head
  scorer is contributing on top of a much stronger signal. Diminishing returns.
- Engineering surface (worker threads, file locks, hot reload, training data
  pipeline, SurrealDB queries, weight versioning, validation) is large for the
  marginal lift over hand-tuned weights + cross-encoder.

If we want learned scoring eventually, doing it after we have benchmark infra
+ retrieval-outcome data + a cross-encoder baseline is the right order.

### Adding more hand-tuned WMR signals

kongcode's WMR has 7 signals; chitta's review notes already flag that score
floors are tuned for cosine-heavy weights. Adding `proven_utility`,
`reflection_boost`, `neighbor_bonus` etc. to chitta before we have eval infra
to verify they help is how you end up with a magic-numbers file no one dares
touch. Resist until benchmarks can score the deltas.

## Suggested order (post-refactor)

1. retrieval-outcome logging (cheap, unblocks everything else)
2. cross-encoder rerank with feature flag, default off
3. measure on PersonaMem / LongMemEval / chitta's own evals
4. only then revisit learned scoring — and only if measurements show
   WMR-or-cosine + cross-encoder has a real ceiling
