"""Pull embeddings from chitta_astrobench and cluster with EVōC."""

import numpy as np
import psycopg
from pgvector.psycopg import register_vector
import evoc
import time

DB = "postgresql://josh:ogham@localhost/chitta_astrobench"

def load_embeddings(profile=None):
    with psycopg.connect(DB) as conn:
        register_vector(conn)
        where = f"WHERE profile = '{profile}'" if profile else ""
        rows = conn.execute(
            f"SELECT id, profile, left(content, 120), embedding FROM memories {where} ORDER BY record_time"
        ).fetchall()

    ids = [r[0] for r in rows]
    profiles = [r[1] for r in rows]
    snippets = [r[2] for r in rows]
    embeddings = np.array([np.array(r[3]) for r in rows], dtype=np.float32)
    return ids, profiles, snippets, embeddings


def main():
    print("Loading embeddings from chitta_astrobench...")
    ids, profiles, snippets, embeddings = load_embeddings()
    print(f"  {embeddings.shape[0]} vectors, {embeddings.shape[1]} dims")
    print(f"  profiles: { {p: profiles.count(p) for p in set(profiles)} }")

    print("\nRunning EVōC clustering...")
    t0 = time.time()
    clusterer = evoc.EVoC()
    labels = clusterer.fit_predict(embeddings)
    elapsed = time.time() - t0
    print(f"  done in {elapsed:.1f}s")

    n_clusters = len(set(labels) - {-1})
    n_noise = (labels == -1).sum()
    print(f"  {n_clusters} clusters found, {n_noise} noise points")

    layers = clusterer.cluster_layers_
    print(f"  {len(layers)} granularity layers")
    for i, layer in enumerate(layers):
        nc = len(set(layer) - {-1})
        nn = (layer == -1).sum()
        print(f"    layer {i}: {nc} clusters, {nn} noise")

    dupes = clusterer.duplicates_
    if dupes is not None and len(dupes) > 0:
        print(f"\n  {len(dupes)} potential duplicate pairs")
        for a, b in list(dupes)[:5]:
            print(f"    [{profiles[a]}] {snippets[a][:60]}")
            print(f"    [{profiles[b]}] {snippets[b][:60]}")
            print()

    # Show a few clusters with their contents
    print("\n--- Sample clusters (finest layer) ---")
    finest = layers[0]
    cluster_ids = sorted(set(finest) - {-1})
    for cid in cluster_ids[:8]:
        members = np.where(finest == cid)[0]
        print(f"\nCluster {cid} ({len(members)} members):")
        for idx in members[:4]:
            print(f"  [{profiles[idx]}] {snippets[idx][:90]}")
        if len(members) > 4:
            print(f"  ... and {len(members) - 4} more")


if __name__ == "__main__":
    main()
