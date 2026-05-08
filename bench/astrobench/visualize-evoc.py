"""Cluster chitta_astrobench embeddings with EVōC, project with UMAP, render with DataMapPlot."""

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
            "SELECT id, profile, content, embedding FROM memories ORDER BY record_time"
        ).fetchall()

    ids = [str(r[0]) for r in rows]
    profiles = [r[1] for r in rows]
    contents = [r[2] for r in rows]
    embeddings = np.array([np.array(r[3]) for r in rows], dtype=np.float32)
    return ids, profiles, contents, embeddings


def label_cluster(indices, contents, profiles, max_words=6):
    texts = [contents[i] for i in indices]
    profs = [profiles[i] for i in indices]
    prof_counts = Counter(profs)
    dominant_profile = prof_counts.most_common(1)[0][0]

    words = []
    for t in texts:
        clean = t.replace("#", "").replace("/", " ").replace("-", " ")
        for w in clean.split():
            w = w.strip(".,;:!?()[]{}\"'").lower()
            if len(w) > 2 and w not in {"the", "and", "for", "that", "this", "with", "from", "are", "was", "has", "have"}:
                words.append(w)

    word_counts = Counter(words)
    top_words = [w for w, _ in word_counts.most_common(max_words)]
    label = " ".join(top_words[:max_words])
    return f"[{dominant_profile}] {label}"


def format_hover(profile, content, mem_id):
    """Format content for readable hover display."""
    clean = content.replace("\n", " ").replace("\r", "")
    # collapse multiple spaces
    while "  " in clean:
        clean = clean.replace("  ", " ")
    # take first 500 chars, break into ~80-char lines
    text = clean[:500]
    lines = []
    while len(text) > 80:
        brk = text.rfind(" ", 0, 80)
        if brk == -1:
            brk = 80
        lines.append(text[:brk])
        text = text[brk:].lstrip()
    if text:
        lines.append(text)
    body = "<br>".join(lines)
    return f"<b>[{profile}]</b> {mem_id[:8]}<br><br>{body}"


def main():
    print("Loading embeddings...")
    ids, profiles, contents, embeddings = load_embeddings()
    print(f"  {len(ids)} vectors, {embeddings.shape[1]} dims")

    print("Running EVōC clustering...")
    t0 = time.time()
    clusterer = evoc.EVoC()
    labels = clusterer.fit_predict(embeddings)
    print(f"  done in {time.time() - t0:.1f}s")

    layers = clusterer.cluster_layers_
    print(f"  {len(layers)} layers: {[len(set(l) - {-1}) for l in layers]} clusters")

    print("Running UMAP projection to 2D...")
    t0 = time.time()
    reducer = umap.UMAP(n_components=2, metric="cosine", random_state=42, n_jobs=1)
    coords_2d = reducer.fit_transform(embeddings)
    print(f"  done in {time.time() - t0:.1f}s")

    # Generate labels for each layer
    label_layers = []
    for layer_idx, layer in enumerate(layers):
        cluster_ids = sorted(set(layer) - {-1})
        label_map = {}
        for cid in cluster_ids:
            members = np.where(layer == cid)[0]
            label_map[cid] = label_cluster(members, contents, profiles)

        text_labels = []
        for l in layer:
            if l == -1:
                text_labels.append("unlabeled")
            else:
                text_labels.append(label_map[l])
        label_layers.append(np.array(text_labels))
        print(f"  layer {layer_idx}: {len(cluster_ids)} labels generated")

    # Pick two layers for DataMapPlot: finest and a mid/coarse layer
    if len(label_layers) >= 2:
        fine_labels = label_layers[0]
        coarse_labels = label_layers[-1]
    else:
        fine_labels = label_layers[0]
        coarse_labels = label_layers[0]

    import pandas as pd
    extra = pd.DataFrame({
        "profile": profiles,
        "content": [c[:800] for c in contents],
        "mem_id": ids,
    })

    hover = np.array([f"[{p}] {c[:80]}" for p, c in zip(profiles, contents)])

    panel_html = """
    <div id="detail-panel" style="
        display:none; position:fixed; right:16px; top:16px; bottom:16px;
        width:380px; background:#1a1a2e; color:#e0e0e0; border:1px solid #444;
        border-radius:8px; padding:20px; overflow-y:auto; z-index:9999;
        font-family:monospace; font-size:13px; line-height:1.5;
        box-shadow: 0 4px 20px rgba(0,0,0,0.5);
    ">
        <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:12px;">
            <span id="panel-header" style="font-weight:bold; font-size:14px; color:#88aaff;"></span>
            <button onclick="document.getElementById('detail-panel').style.display='none'"
                style="background:none; border:none; color:#888; cursor:pointer; font-size:18px;">✕</button>
        </div>
        <div id="panel-body" style="white-space:pre-wrap; word-break:break-word;"></div>
    </div>
    """

    on_click_js = """
        var panel = document.getElementById('detail-panel');
        var header = document.getElementById('panel-header');
        var body = document.getElementById('panel-body');
        header.textContent = '[' + {profile} + '] ' + {mem_id}.substring(0, 8);
        body.textContent = {content};
        panel.style.display = 'block';
    """

    print("Rendering interactive DataMapPlot...")
    fig = datamapplot.create_interactive_plot(
        coords_2d,
        coarse_labels,
        fine_labels,
        hover_text=hover,
        extra_point_data=extra,
        on_click=on_click_js,
        custom_html=panel_html,
        title="chitta_astrobench embedding space",
        sub_title=f"{len(ids)} memories — astro / cards / iching",
        noise_label="unlabeled",
        enable_search=True,
        search_field="content",
        darkmode=True,
        point_radius_min_pixels=1.5,
        point_radius_max_pixels=12,
    )

    out_path = "outputs/astrobench-map.html"
    fig.save(out_path)
    print(f"  saved to {out_path}")
    print(f"  open with: xdg-open {out_path}")


if __name__ == "__main__":
    main()
