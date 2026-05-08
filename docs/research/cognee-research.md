# Cognee research — what's relevant for chitta

> From 2026-05-07 session. Cognee is an open-source knowledge graph engine
> that goes beyond flat vector retrieval.

---

## What Cognee does

Pipeline: add (ingest) -> cognify (LLM entity/relationship extraction into
graph) -> search (multi-hop graph traversal) -> memify (feedback-driven
weight updates).

Graph backends: Neo4j, Neptune, Postgres, or built-in Ladybug.
Vector backends: LanceDB, ChromaDB, PGVector, Qdrant, Weaviate, Milvus.

---

## Relevant mechanisms for chitta

### 1. `memify` — retrieval-outcome feedback weights

Cognee ingests agent trace feedback, re-runs cognify on it to incorporate
into the graph, then applies streaming feedback-weight updates to existing
graph nodes. Formula: normalized 1-5 score -> exponential moving average.
Nodes that lead to good agent outcomes get reinforced over time. Also has
frequency-weight tasks reinforcing nodes accessed more often.

**Chitta connection:** This is almost exactly what the "retrieval-outcome
feedback" design memo describes (chitta memory `716c3fe4`). Cognee has a
working implementation of what chitta planned: per-profile retriever
improvement with zero LLM retraining, by tracking which retrieved memories
lead to good outcomes.

Differences from chitta's planned approach:
- Cognee uses EMA on explicit 1-5 scores; chitta's memo proposed tracking
  whether retrieved memories were actually used by the agent (implicit
  signal, no explicit rating needed)
- Cognee's feedback goes through the full cognify pipeline (LLM call);
  chitta's approach avoids LLM on the feedback path
- Both arrive at "reinforce useful nodes, decay unused ones"

### 2. Sessions as graph subgraphs

Agent trace sessions are persisted as first-class graph subgraphs via
`persist_sessions_in_knowledge_graph.py`. Conversation history becomes
traversable graph structure, not just logs.

**Chitta connection:** Chitta stores observations from sessions but doesn't
structurally link them. A session is currently a bag of observations tagged
with `source: claude-code`. If chitta's entity graph materializes, session
context could become a subgraph — observations linked to the entities they
mention, decisions linked to the alternatives they rejected, etc. This
would make "what did we discuss about X last week" a graph traversal
instead of a semantic search.

### 3. Multiple retrieval strategies

Cognee offers ~12 retrieval modes:
- `GRAPH_COMPLETION` — graph traversal + LLM synthesis
- `TRIPLET_COMPLETION` — subject-predicate-object lookup
- `GRAPH_COMPLETION_COT` — chain-of-thought over graph
- `TEMPORAL` — time-aware graph search
- `RAG_COMPLETION` — fallback flat vector RAG
- `CHUNKS` / `CHUNKS_LEXICAL` — pure vector or keyword
- `CYPHER` — raw graph queries
- `FEELING_LUCKY` — auto-selects

**Chitta connection:** Chitta currently has dense (vector) retrieval only.
The research branch plans entity graphs. When that lands, chitta could
expose retrieval strategy selection — let the caller specify "I want graph
traversal" vs "I want temporal" vs "just find the nearest vector." This
is more useful than a single `search_memories` endpoint that always does
the same thing.

### 4. Pre-computed summaries as graph nodes

Cognee stores document summaries as graph nodes, enabling summary-grounded
traversal. Instead of retrieving raw chunks and hoping the agent synthesizes,
the graph already contains condensed versions.

**Chitta connection:** Maps to chitta's `mental_model` memory type. Mental
models are already meant to be consolidated, higher-level artifacts. The
graph could link mental models to the observations they were synthesized
from, making the provenance traversable.

---

## What NOT to adopt from Cognee

- **LLM on every write path.** Cognee's cognify step runs LLM extraction
  on every ingested document. Chitta's principle is no LLM on the write
  path. The async enrichment queue (from OB1 research, already in
  innovation-potentials.md) is the right compromise.
- **External graph DB dependency.** Cognee supports Neo4j/Neptune but also
  has a built-in option. Chitta should keep the graph in SQLite (or a
  lightweight extension) rather than requiring a separate graph database.
- **Multi-tenant SaaS architecture.** Cognee has ACL-based permissions and
  dataset isolation for multi-user. Chitta is single-user with profile
  isolation — simpler and sufficient.
