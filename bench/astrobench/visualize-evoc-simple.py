"""Minimal EVōC + UMAP + DataMapPlot — no extra_point_data, just hover text."""

import numpy as np
import psycopg
from pgvector.psycopg import register_vector
import evoc
import umap
import datamapplot
import time
from collections import Counter

DB = "postgresql://josh:ogham@localhost/chitta_astrobench"


def load_embeddings():
    with psycopg.connect(DB) as conn:
        register_vector(conn)
        rows = conn.execute(
            "SELECT id, profile, left(content, 200), embedding FROM memories ORDER BY record_time"
        ).fetchall()

    ids = [str(r[0]) for r in rows]
    profiles = [r[1] for r in rows]
    snippets = [r[2] for r in rows]
    embeddings = np.array([np.array(r[3]) for r in rows], dtype=np.float32)
    return ids, profiles, snippets, embeddings


def label_cluster(indices, snippets, profiles, max_words=5):
    texts = [snippets[i] for i in indices]
    profs = [profiles[i] for i in indices]
    dominant = Counter(profs).most_common(1)[0][0]

    words = []
    stop = {"the", "and", "for", "that", "this", "with", "from", "are", "was", "has", "have", "not", "its", "all"}
    for t in texts:
        for w in t.replace("#", " ").replace("/", " ").replace("-", " ").split():
            w = w.strip(".,;:!?()[]{}\"'|+").lower()
            if len(w) > 2 and w not in stop and not w.startswith("http"):
                words.append(w)

    top = [w for w, _ in Counter(words).most_common(max_words)]
    return f"[{dominant}] {' '.join(top)}"


def main():
    print("Loading embeddings...")
    ids, profiles, snippets, embeddings = load_embeddings()
    print(f"  {len(ids)} vectors, {embeddings.shape[1]} dims")

    print("Running EVōC...")
    t0 = time.time()
    clusterer = evoc.EVoC()
    clusterer.fit_predict(embeddings)
    print(f"  done in {time.time() - t0:.1f}s")

    layers = clusterer.cluster_layers_
    print(f"  {len(layers)} layers: {[len(set(l) - {-1}) for l in layers]} clusters")

    print("Running UMAP...")
    t0 = time.time()
    coords = umap.UMAP(n_components=2, metric="cosine", random_state=42, n_jobs=1).fit_transform(embeddings)
    print(f"  done in {time.time() - t0:.1f}s")

    label_arrays = []
    for layer in layers:
        cids = sorted(set(layer) - {-1})
        lmap = {}
        for cid in cids:
            members = np.where(layer == cid)[0]
            lmap[cid] = label_cluster(members, snippets, profiles)
        label_arrays.append(np.array(["unlabeled" if l == -1 else lmap[l] for l in layer]))

    # clean hover text — ASCII-safe, short
    hover = np.array([
        f"[{p}] {s[:120].encode('ascii', 'replace').decode()}"
        for p, s in zip(profiles, snippets)
    ])

    print("Rendering...")
    fig = datamapplot.create_interactive_plot(
        coords,
        label_arrays[-1],  # coarsest
        label_arrays[0],   # finest
        hover_text=hover,
        title="chitta_astrobench embedding space",
        sub_title=f"{len(ids)} memories",
        noise_label="unlabeled",
        enable_search=True,
        darkmode=True,
    )

    out = "outputs/astrobench-map.html"
    fig.save(out)
    print(f"  saved to {out}")


if __name__ == "__main__":
    main()
