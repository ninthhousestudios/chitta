#!/usr/bin/env python3
"""Head-to-head embedding quality comparison: BGE-M3 vs Qwen3-VL-Embedding-2B.

No DB needed. Directly embeds queries and documents with both models,
compares cosine similarity separation between relevant and irrelevant docs.

Usage:
    # From the Qwen venv (has torch + transformers):
    /home/josh/soft/Qwen3-VL-Embedding/.venv/bin/python compare-embedders.py
"""
from __future__ import annotations

import json
import random
import sys
import time
from pathlib import Path

import numpy as np

VAULT_DIR = Path.home() / "vault"
QUERY_DIR = Path(__file__).resolve().parent.parent / "datasets" / "astrobench" / "queries"
N_QUERIES_PER_SLICE = 3
N_DISTRACTORS = 5
SEED = 42


def cosine_sim(a: list[float], b: list[float]) -> float:
    a_np = np.array(a)
    b_np = np.array(b)
    return float(np.dot(a_np, b_np) / (np.linalg.norm(a_np) * np.linalg.norm(b_np) + 1e-9))


def load_queries(query_dir: Path, n_per_slice: int) -> list[dict]:
    rng = random.Random(SEED)
    queries = []
    for f in sorted(query_dir.glob("slice-*.jsonl")):
        slice_queries = []
        for line in f.read_text().splitlines():
            if line.strip():
                slice_queries.append(json.loads(line))
        sampled = rng.sample(slice_queries, min(n_per_slice, len(slice_queries)))
        queries.extend(sampled)
    return queries


def collect_all_docs(query_dir: Path) -> list[str]:
    """Collect all gold doc paths across all queries for distractor sampling."""
    paths = set()
    for f in sorted(query_dir.glob("slice-*.jsonl")):
        for line in f.read_text().splitlines():
            if line.strip():
                q = json.loads(line)
                paths.update(q["gold_chunk_ids"])
    return sorted(paths)


def read_doc(vault_dir: Path, rel_path: str, max_chars: int = 2000) -> str | None:
    # gold_chunk_ids are relative to profile subdirs — try astro first
    for profile in ["astro", "iching", "cards"]:
        full = vault_dir / profile / rel_path
        if full.exists():
            return full.read_text(encoding="utf-8")[:max_chars]
    return None


def run_comparison():
    queries = load_queries(QUERY_DIR, N_QUERIES_PER_SLICE)
    all_doc_paths = collect_all_docs(QUERY_DIR)
    rng = random.Random(SEED)

    # Prepare test cases: (query_text, gold_doc_text, [distractor_texts])
    test_cases = []
    for q in queries:
        gold_path = q["gold_chunk_ids"][0]
        gold_text = read_doc(VAULT_DIR, gold_path)
        if not gold_text:
            continue

        # Pick distractors: random docs NOT in this query's gold set
        gold_set = set(q["gold_chunk_ids"])
        distractor_paths = [p for p in all_doc_paths if p not in gold_set]
        distractor_sample = rng.sample(distractor_paths, min(N_DISTRACTORS, len(distractor_paths)))
        distractor_texts = []
        for dp in distractor_sample:
            dt = read_doc(VAULT_DIR, dp)
            if dt:
                distractor_texts.append(dt)

        test_cases.append({
            "id": q["id"],
            "slice": q["slice"],
            "query": q["query"],
            "gold_text": gold_text,
            "distractors": distractor_texts,
        })

    print(f"Prepared {len(test_cases)} test cases with {N_DISTRACTORS} distractors each")

    # Collect all unique texts to embed (avoid re-embedding duplicates)
    all_texts = set()
    for tc in test_cases:
        all_texts.add(tc["query"])
        all_texts.add(tc["gold_text"])
        for d in tc["distractors"]:
            all_texts.add(d)
    all_texts = sorted(all_texts)
    print(f"Total unique texts to embed: {len(all_texts)}")

    results = {}
    for model_name, load_fn in [("bge-m3", load_bge), ("qwen3-vl", load_qwen)]:
        print(f"\n{'='*60}")
        print(f"  Embedding with {model_name}")
        print(f"{'='*60}")

        embedder = load_fn()

        # Embed all texts
        embeddings = {}
        t0 = time.perf_counter()
        for i, text in enumerate(all_texts):
            dense, _ = embedder.embed(text[:8000])
            embeddings[text] = dense
            if (i + 1) % 10 == 0 or i + 1 == len(all_texts):
                elapsed = time.perf_counter() - t0
                rate = (i + 1) / elapsed
                print(f"  {i+1}/{len(all_texts)} embedded ({rate:.1f}/s, {elapsed:.0f}s elapsed)")

        total_time = time.perf_counter() - t0
        print(f"  Done: {len(all_texts)} embeddings in {total_time:.1f}s")

        # Score each test case
        model_results = []
        for tc in test_cases:
            q_emb = embeddings[tc["query"]]
            gold_sim = cosine_sim(q_emb, embeddings[tc["gold_text"]])

            dist_sims = [cosine_sim(q_emb, embeddings[d]) for d in tc["distractors"]]
            mean_dist = np.mean(dist_sims) if dist_sims else 0.0
            max_dist = max(dist_sims) if dist_sims else 0.0

            # Gold rank among all candidates
            all_sims = [(gold_sim, "gold")] + [(s, "dist") for s in dist_sims]
            all_sims.sort(key=lambda x: -x[0])
            gold_rank = next(i for i, (_, label) in enumerate(all_sims, 1) if label == "gold")

            model_results.append({
                "id": tc["id"],
                "slice": tc["slice"],
                "gold_sim": gold_sim,
                "mean_dist_sim": float(mean_dist),
                "max_dist_sim": float(max_dist),
                "separation": gold_sim - float(mean_dist),
                "gold_rank": gold_rank,
            })

        results[model_name] = model_results

    # Print comparison
    print(f"\n{'='*70}")
    print(f"  HEAD-TO-HEAD COMPARISON")
    print(f"{'='*70}")

    header = f"  {'id':8s} {'slice':5s}  {'':11s}  {'BGE-M3':>8s}  {'Qwen3-VL':>8s}  {'winner':>8s}"
    print(header)
    print(f"  {'-'*64}")

    bge_wins = 0
    qwen_wins = 0
    ties = 0

    for bge_r, qwen_r in zip(results["bge-m3"], results["qwen3-vl"]):
        b_sep = bge_r["separation"]
        q_sep = qwen_r["separation"]
        if b_sep > q_sep + 0.01:
            winner = "BGE"
            bge_wins += 1
        elif q_sep > b_sep + 0.01:
            winner = "Qwen"
            qwen_wins += 1
        else:
            winner = "tie"
            ties += 1

        print(f"  {bge_r['id']:8s} {bge_r['slice']:5s}  gold_sim    {bge_r['gold_sim']:8.4f}  {qwen_r['gold_sim']:8.4f}")
        print(f"  {'':8s} {'':5s}  mean_dist   {bge_r['mean_dist_sim']:8.4f}  {qwen_r['mean_dist_sim']:8.4f}")
        print(f"  {'':8s} {'':5s}  separation  {b_sep:8.4f}  {q_sep:8.4f}  {winner:>8s}")
        print(f"  {'':8s} {'':5s}  gold_rank   {bge_r['gold_rank']:8d}  {qwen_r['gold_rank']:8d}")
        print()

    print(f"  {'='*64}")
    n = len(results["bge-m3"])

    def agg(key, model):
        vals = [r[key] for r in results[model]]
        return np.mean(vals)

    print(f"  {'AGGREGATE':14s}  {'BGE-M3':>8s}  {'Qwen3-VL':>8s}")
    print(f"  {'mean gold_sim':14s}  {agg('gold_sim', 'bge-m3'):8.4f}  {agg('gold_sim', 'qwen3-vl'):8.4f}")
    print(f"  {'mean dist_sim':14s}  {agg('mean_dist_sim', 'bge-m3'):8.4f}  {agg('mean_dist_sim', 'qwen3-vl'):8.4f}")
    print(f"  {'mean separat.':14s}  {agg('separation', 'bge-m3'):8.4f}  {agg('separation', 'qwen3-vl'):8.4f}")
    print(f"  {'mean rank':14s}  {agg('gold_rank', 'bge-m3'):8.2f}  {agg('gold_rank', 'qwen3-vl'):8.2f}")
    print(f"  {'MRR':14s}  {np.mean([1/r['gold_rank'] for r in results['bge-m3']]):8.4f}  {np.mean([1/r['gold_rank'] for r in results['qwen3-vl']]):8.4f}")
    print(f"\n  Wins: BGE-M3={bge_wins}  Qwen3-VL={qwen_wins}  ties={ties}  (n={n})")


def load_bge():
    sys.path.insert(0, str(Path(__file__).parent))
    from embedder import Embedder
    return Embedder()


def load_qwen():
    sys.path.insert(0, str(Path(__file__).parent))
    from embedder_qwen import Embedder
    return Embedder()


if __name__ == "__main__":
    run_comparison()
