#!/usr/bin/env python3
"""Retrieval evaluation for astrobench.

Reimplements chitta-rs's retrieval pipeline (dense ANN + FTS + sparse + RRF)
directly in Python against the chitta_astrobench database. This avoids needing
a running server and lets us sweep configs without restarts.

Usage:
    cd bench/astrobench
    uv run python eval.py --config dense-only
    uv run python eval.py --config dense-sparse
    uv run python eval.py --config dense-sparse-fts

    # sweep all three
    uv run python eval.py --sweep
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from collections import defaultdict
from pathlib import Path

import psycopg
from pgvector.psycopg import register_vector

def _load_bge_embedder():
    from embedder import Embedder
    return Embedder

DEFAULT_DB_URL = "postgresql://josh:ogham@localhost/chitta_astrobench"
QUERY_DIR = Path(__file__).resolve().parent.parent / "datasets" / "astrobench" / "queries"

CONFIGS = {
    "dense-only": {"rrf_fts": False, "rrf_sparse": False},
    "dense-sparse": {"rrf_fts": False, "rrf_sparse": True},
    "dense-sparse-fts": {"rrf_fts": True, "rrf_sparse": True},
}

RRF_K = 60
RRF_CANDIDATES = 5


def load_queries(query_dir: Path, slices: list[str] | None = None) -> list[dict]:
    queries = []
    for f in sorted(query_dir.glob("slice-*.jsonl")):
        slice_name = f.stem.replace("slice-", "")
        if slices and slice_name not in slices:
            continue
        for line_no, line in enumerate(f.read_text().splitlines(), 1):
            line = line.strip()
            if not line:
                continue
            try:
                q = json.loads(line)
                q.setdefault("slice", slice_name)
                queries.append(q)
            except json.JSONDecodeError as e:
                print(f"  warning: {f.name}:{line_no} bad JSON: {e}")
    return queries


def search_dense(
    conn: psycopg.Connection,
    profile: str,
    query_vec: list[float],
    limit: int,
) -> list[tuple[str, float]]:
    """Dense ANN search via pgvector. Returns [(id, similarity)]."""
    with conn.cursor() as cur:
        cur.execute(
            f"SET LOCAL hnsw.ef_search = {max(200, limit * 4)}"
        )
        cur.execute(
            """
            SELECT id::text,
                   (1.0 - (embedding <=> %s::vector))::real AS similarity
            FROM memories
            WHERE profile = %s
            ORDER BY embedding <=> %s::vector
            LIMIT %s
            """,
            (str(query_vec), profile, str(query_vec), limit),
        )
        return [(row[0], row[1]) for row in cur.fetchall()]


def search_fts(
    conn: psycopg.Connection,
    profile: str,
    query_text: str,
    limit: int,
) -> list[str]:
    """FTS search. Returns [id] ranked by ts_rank."""
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT id::text
            FROM memories
            WHERE profile = %s
              AND content_tsvector @@ plainto_tsquery('english', %s)
            ORDER BY ts_rank(content_tsvector, plainto_tsquery('english', %s)) DESC
            LIMIT %s
            """,
            (profile, query_text, query_text, limit),
        )
        return [row[0] for row in cur.fetchall()]


def fetch_sparse_embeddings(
    conn: psycopg.Connection,
    ids: list[str],
) -> dict[str, dict[int, float]]:
    """Fetch sparse embeddings for given IDs."""
    if not ids:
        return {}
    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT id::text, sparse_embedding
            FROM memories
            WHERE id = ANY(%s::uuid[])
              AND sparse_embedding IS NOT NULL
            """,
            (ids,),
        )
        result = {}
        for row_id, sparse_json in cur.fetchall():
            if sparse_json:
                result[row_id] = {
                    int(k): float(v) for k, v in sparse_json.items()
                }
        return result


def sparse_dot(
    query_sparse: dict[int, float],
    doc_sparse: dict[int, float],
) -> float:
    return sum(
        qw * doc_sparse[tok]
        for tok, qw in query_sparse.items()
        if tok in doc_sparse
    )


def retrieve(
    conn: psycopg.Connection,
    embedder: Embedder,
    profile: str,
    query_text: str,
    k: int,
    config: dict,
) -> tuple[list[tuple[str, float]], float]:
    """Run retrieval pipeline. Returns ([(id, score)], latency_ms)."""
    t0 = time.perf_counter()

    dense_vec, query_sparse = embedder.embed(query_text)
    fetch_limit = k * RRF_CANDIDATES

    with conn.transaction():
        dense_results = search_dense(conn, profile, dense_vec, fetch_limit)

        fts_ids = []
        if config["rrf_fts"]:
            fts_ids = search_fts(conn, profile, query_text, fetch_limit)

    # If dense-only, just return dense results directly
    if not config["rrf_fts"] and not config["rrf_sparse"]:
        latency = (time.perf_counter() - t0) * 1000
        return dense_results[:k], latency

    # RRF fusion
    rrf_scores: dict[str, float] = defaultdict(float)
    k_const = float(RRF_K)

    for rank, (doc_id, _sim) in enumerate(dense_results):
        rrf_scores[doc_id] += 1.0 / (k_const + rank)

    for rank, doc_id in enumerate(fts_ids):
        rrf_scores[doc_id] += 1.0 / (k_const + rank)

    # Sparse re-ranking
    if config["rrf_sparse"] and query_sparse:
        candidate_ids = list(rrf_scores.keys())
        sparse_docs = fetch_sparse_embeddings(conn, candidate_ids)

        scored = [
            (doc_id, sparse_dot(query_sparse, doc_sparse))
            for doc_id, doc_sparse in sparse_docs.items()
        ]
        scored.sort(key=lambda x: -x[1])

        for rank, (doc_id, _dot) in enumerate(scored):
            rrf_scores[doc_id] += 1.0 / (k_const + rank)

    ranked = sorted(rrf_scores.items(), key=lambda x: -x[1])[:k]

    latency = (time.perf_counter() - t0) * 1000
    return ranked, latency


def build_source_index(
    conn: psycopg.Connection,
    profile: str,
) -> tuple[dict[str, str], dict[str, set[str]]]:
    """Build UUID→source_path and source_path→{UUIDs} mappings."""
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id::text, metadata->>'source_path' FROM memories WHERE profile = %s",
            (profile,),
        )
        id_to_source: dict[str, str] = {}
        source_to_ids: dict[str, set[str]] = defaultdict(set)
        for row_id, source_path in cur.fetchall():
            id_to_source[row_id] = source_path or ""
            if source_path:
                source_to_ids[source_path].add(row_id)
    return id_to_source, dict(source_to_ids)


def evaluate_query(
    conn: psycopg.Connection,
    embedder: Embedder,
    query: dict,
    profile: str,
    k_values: list[int],
    config: dict,
    id_to_source: dict[str, str] | None = None,
) -> dict:
    max_k = max(k_values)
    results, latency = retrieve(conn, embedder, profile, query["query"], max_k, config)

    retrieved_ids = [doc_id for doc_id, _score in results]
    gold_ids = query["gold_chunk_ids"]

    # Detect whether gold IDs are source_paths (contain '/') or UUIDs
    uses_paths = any("/" in g for g in gold_ids)

    if uses_paths and id_to_source:
        # File-level matching: map retrieved UUIDs to source_paths
        retrieved_sources = [id_to_source.get(rid, "") for rid in retrieved_ids]

        gold_sources = set(gold_ids)
        metrics = {"retrieve_ms": round(latency, 1)}
        for k in k_values:
            top_k_sources = set(retrieved_sources[:k])
            matched = gold_sources & top_k_sources
            recall = len(matched) / len(gold_sources) if gold_sources else 0.0
            metrics[f"recall@{k}"] = round(recall, 4)

        mrr = 0.0
        for rank, src in enumerate(retrieved_sources, 1):
            if src in gold_sources:
                mrr = 1.0 / rank
                break
        metrics["mrr"] = round(mrr, 4)

        metrics["hit"] = 1.0 if gold_sources & set(retrieved_sources) else 0.0
    else:
        # Direct UUID matching (original behavior)
        gold_set = set(gold_ids)
        metrics = {"retrieve_ms": round(latency, 1)}
        for k in k_values:
            top_k = set(retrieved_ids[:k])
            matched = gold_set & top_k
            recall = len(matched) / len(gold_set) if gold_set else 0.0
            metrics[f"recall@{k}"] = round(recall, 4)

        mrr = 0.0
        for rank, doc_id in enumerate(retrieved_ids, 1):
            if doc_id in gold_set:
                mrr = 1.0 / rank
                break
        metrics["mrr"] = round(mrr, 4)

        metrics["hit"] = 1.0 if gold_set & set(retrieved_ids) else 0.0

    return {
        "query_id": query["id"],
        "slice": query.get("slice", "unknown"),
        "query": query["query"][:200],
        "gold_chunk_ids": query["gold_chunk_ids"],
        "retrieved_ids": retrieved_ids[:20],
        **metrics,
    }


def run_eval(
    conn: psycopg.Connection,
    embedder: Embedder,
    queries: list[dict],
    profile: str,
    k_values: list[int],
    config: dict,
    config_name: str,
    id_to_source: dict[str, str] | None = None,
) -> dict:
    results = []
    total = len(queries)
    for i, q in enumerate(queries):
        results.append(evaluate_query(conn, embedder, q, profile, k_values, config, id_to_source))
        if (i + 1) % 10 == 0 or i + 1 == total:
            print(f"  {i + 1}/{total} queries evaluated")

    # Aggregate metrics
    def mean(key):
        vals = [r[key] for r in results if key in r]
        return round(sum(vals) / len(vals), 4) if vals else 0.0

    metrics = {}
    for k in k_values:
        metrics[f"mean_recall@{k}"] = mean(f"recall@{k}")
    metrics["hit_rate"] = mean("hit")
    metrics["mean_mrr"] = mean("mrr")
    metrics["mean_retrieve_ms"] = round(mean("retrieve_ms"), 1)

    # Per-slice breakdown
    by_slice = defaultdict(list)
    for r in results:
        by_slice[r["slice"]].append(r)

    slice_metrics = {}
    for s, s_results in sorted(by_slice.items()):
        n = len(s_results)
        sm = {"count": n}
        for k in k_values:
            sm[f"mean_recall@{k}"] = round(
                sum(r[f"recall@{k}"] for r in s_results) / n, 4
            )
        sm["hit_rate"] = round(sum(r["hit"] for r in s_results) / n, 4)
        sm["mean_mrr"] = round(sum(r["mrr"] for r in s_results) / n, 4)
        sm["mean_retrieve_ms"] = round(
            sum(r["retrieve_ms"] for r in s_results) / n, 1
        )
        slice_metrics[s] = sm

    return {
        "config_name": config_name,
        "config": config,
        "rrf_k": RRF_K,
        "rrf_candidates": RRF_CANDIDATES,
        "k_values": k_values,
        "profile": profile,
        "total_queries": total,
        "metrics": metrics,
        "slice_metrics": slice_metrics,
        "results": results,
    }


def print_report(summary: dict):
    m = summary["metrics"]
    cfg = summary["config_name"]
    legs = ["dense"]
    if summary["config"].get("rrf_fts"):
        legs.append("fts")
    if summary["config"].get("rrf_sparse"):
        legs.append("sparse")

    print(f"\n{'=' * 70}")
    print(f"  config: {cfg}  |  legs: {'+'.join(legs)}  |  "
          f"rrf_k={summary['rrf_k']}  |  {summary['total_queries']} queries")
    print(f"{'=' * 70}")

    for k in summary["k_values"]:
        key = f"mean_recall@{k}"
        print(f"  recall@{k:2d}:  {m[key]:.1%}")
    print(f"  hit rate:  {m['hit_rate']:.1%}")
    print(f"  MRR:       {m['mean_mrr']:.4f}")
    print(f"  latency:   {m['mean_retrieve_ms']:.1f}ms")

    if summary.get("slice_metrics"):
        print(f"\n  Per-slice breakdown:")
        header = f"    {'slice':8s}  {'n':>3s}"
        for k in summary["k_values"]:
            header += f"  {'r@'+str(k):>6s}"
        header += f"  {'hit':>5s}  {'mrr':>6s}  {'ms':>6s}"
        print(header)
        print(f"    {'-' * (len(header) - 4)}")
        for s, sm in sorted(summary["slice_metrics"].items()):
            line = f"    {s:8s}  {sm['count']:3d}"
            for k in summary["k_values"]:
                line += f"  {sm[f'mean_recall@{k}']:6.1%}"
            line += f"  {sm['hit_rate']:5.1%}  {sm['mean_mrr']:6.4f}  {sm['mean_retrieve_ms']:6.1f}"
            print(line)
    print()


def save_results(summary: dict, output_dir: Path):
    path = output_dir / f"{summary['config_name']}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(summary, indent=2))
    print(f"  saved → {path}")


def main():
    p = argparse.ArgumentParser(description="Astrobench retrieval evaluation")
    p.add_argument("--db", default=DEFAULT_DB_URL)
    p.add_argument("--profile", default="astro")
    p.add_argument("--query-dir", default=str(QUERY_DIR))
    p.add_argument("--slices", nargs="+", default=None,
                   help="Evaluate only these slices (a, b, c, d)")
    p.add_argument("--config", choices=list(CONFIGS.keys()), default=None,
                   help="Single config to run")
    p.add_argument("--sweep", action="store_true",
                   help="Run all three configs")
    p.add_argument("--k", nargs="+", type=int, default=[5, 10, 20],
                   help="k values for recall@k")
    p.add_argument("--model-dir", default=None,
                   help="Model directory (auto-selected per embedder if omitted)")
    p.add_argument("--embedder", choices=["bge", "qwen"], default="bge",
                   help="Which embedder to use (default: bge)")
    p.add_argument("--output-dir", "-o", default="outputs")
    args = p.parse_args()

    if not args.config and not args.sweep:
        print("Error: specify --config or --sweep", file=sys.stderr)
        sys.exit(1)

    query_dir = Path(args.query_dir)
    queries = load_queries(query_dir, args.slices)
    if not queries:
        print(f"No queries found in {query_dir}", file=sys.stderr)
        sys.exit(1)

    print(f"Loaded {len(queries)} queries from {query_dir}")

    configs_to_run = list(CONFIGS.keys()) if args.sweep else [args.config]

    if args.embedder == "qwen":
        from embedder_qwen import Embedder as QwenEmbedder, DEFAULT_MODEL_DIR as QWEN_DEFAULT
        model_dir = Path(args.model_dir) if args.model_dir else QWEN_DEFAULT
        print(f"Loading Qwen3-VL-Embedding-2B embedder from {model_dir} ...")
        t0 = time.perf_counter()
        embedder = QwenEmbedder(model_dir=model_dir)
    else:
        BGEEmbedder = _load_bge_embedder()
        from embedder import DEFAULT_MODEL_DIR as BGE_DEFAULT
        model_dir = Path(args.model_dir) if args.model_dir else BGE_DEFAULT
        print(f"Loading BGE-M3 embedder from {model_dir} ...")
        t0 = time.perf_counter()
        embedder = BGEEmbedder(model_dir=model_dir)
    print(f"  loaded in {time.perf_counter() - t0:.1f}s")

    conn = psycopg.connect(args.db, autocommit=False)
    register_vector(conn)

    print("Building source index ...")
    id_to_source, _source_to_ids = build_source_index(conn, args.profile)
    print(f"  indexed {len(id_to_source)} chunks")

    for config_name in configs_to_run:
        config = CONFIGS[config_name]
        print(f"\n--- Running config: {config_name} ---")

        summary = run_eval(
            conn, embedder, queries, args.profile, args.k, config, config_name,
            id_to_source=id_to_source,
        )
        print_report(summary)
        save_results(summary, Path(args.output_dir))

    conn.close()


if __name__ == "__main__":
    main()
