#!/usr/bin/env python3
"""Ingest markdown files into chitta_astrobench.

Chunks files by BGE-M3 token count (512 tokens, 64 overlap), computes
dense+sparse embeddings, and inserts directly into Postgres. Idempotent
via content-hash keyed ON CONFLICT DO NOTHING.

Usage:
    cd bench/astrobench
    uv run python ingest.py

    # custom paths / DB
    uv run python ingest.py \
        --db "postgresql://josh:ogham@localhost/chitta_astrobench" \
        --vault ~/vault \
        --chunk-size 512 --chunk-overlap 64
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

import psycopg
from pgvector.psycopg import register_vector

def _load_bge_embedder():
    from embedder import Embedder
    return Embedder

PROFILES = {
    "astro": "astro",
    "iching": "iching",
    "cards": "cards",
}

DEFAULT_DB_URL = "postgresql://josh:ogham@localhost/chitta_astrobench"

INSERT_SQL = """\
INSERT INTO memories (
    id, profile, content, embedding, sparse_embedding,
    event_time, record_time, tags, idempotency_key,
    source, metadata, memory_type
) VALUES (
    %(id)s, %(profile)s, %(content)s, %(embedding)s, %(sparse_embedding)s,
    %(event_time)s, %(record_time)s, %(tags)s, %(idempotency_key)s,
    %(source)s, %(metadata)s, %(memory_type)s
) ON CONFLICT (profile, idempotency_key) DO NOTHING
"""


def uuid7_from_time(dt: datetime) -> str:
    """Generate a UUIDv7-ish ID using unix_ms + random."""
    ms = int(dt.timestamp() * 1000)
    rand_bits = uuid.uuid4().int & ((1 << 74) - 1)
    u = (ms << 80) | (0x7 << 76) | rand_bits
    return str(uuid.UUID(int=u))


def find_md_files(directory: Path) -> list[Path]:
    files = sorted(directory.rglob("*.md"))
    return files


def chunk_tokens(
    token_ids: list[int],
    chunk_size: int,
    chunk_overlap: int,
) -> list[list[int]]:
    """Slide a window of chunk_size tokens with chunk_overlap overlap."""
    if len(token_ids) <= chunk_size:
        return [token_ids]

    stride = chunk_size - chunk_overlap
    chunks = []
    start = 0
    while start < len(token_ids):
        end = min(start + chunk_size, len(token_ids))
        chunks.append(token_ids[start:end])
        if end == len(token_ids):
            break
        start += stride
    return chunks


def content_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:32]


def sparse_to_jsonb(sparse: dict[int, float]) -> str:
    return json.dumps({str(k): round(v, 6) for k, v in sparse.items()})


def ingest_profile(
    conn: psycopg.Connection,
    embedder: Embedder,
    profile: str,
    vault_dir: Path,
    chunk_size: int,
    chunk_overlap: int,
) -> dict:
    md_files = find_md_files(vault_dir)
    if not md_files:
        print(f"  [{profile}] no .md files found in {vault_dir}")
        return {"files": 0, "chunks": 0, "skipped": 0, "errors": 0}

    stats = {"files": len(md_files), "chunks": 0, "skipped": 0, "errors": 0}
    now = datetime.now(timezone.utc)

    for fi, md_path in enumerate(md_files):
        rel_path = str(md_path.relative_to(vault_dir))
        doc_id = md_path.stem

        try:
            text = md_path.read_text(encoding="utf-8")
        except Exception as e:
            print(f"  [{profile}] skip {rel_path}: {e}")
            stats["errors"] += 1
            continue

        if not text.strip():
            continue

        # Tokenize WITHOUT special tokens — chunking operates on content tokens only.
        # Each chunk gets its own CLS/SEP via embed_chunk.
        raw_ids = embedder.tokenize_raw(text)
        content_chunk_size = chunk_size - 2  # reserve 2 for CLS + SEP
        token_chunks = chunk_tokens(raw_ids, content_chunk_size, chunk_overlap)

        for ci, chunk_ids in enumerate(token_chunks):
            chunk_text = embedder.decode_chunk(chunk_ids)
            if not chunk_text.strip():
                continue

            idem_key = f"astrobench:{profile}:{content_hash(chunk_text)}"

            try:
                dense, sparse = embedder.embed_chunk(chunk_ids)
            except Exception as e:
                print(f"  [{profile}] embed error {rel_path} chunk {ci}: {e}")
                stats["errors"] += 1
                continue

            row_id = uuid7_from_time(now)
            metadata = json.dumps({
                "doc_id": doc_id,
                "source_path": rel_path,
                "chunk_index": ci,
                "chunk_count": len(token_chunks),
            })

            try:
                with conn.cursor() as cur:
                    cur.execute(INSERT_SQL, {
                        "id": row_id,
                        "profile": profile,
                        "content": chunk_text,
                        "embedding": dense,
                        "sparse_embedding": sparse_to_jsonb(sparse),
                        "event_time": now,
                        "record_time": now,
                        "tags": ["astrobench"],
                        "idempotency_key": idem_key,
                        "source": "astrobench-ingest",
                        "metadata": metadata,
                        "memory_type": "memory",
                    })
                    if cur.rowcount == 0:
                        stats["skipped"] += 1
                    else:
                        stats["chunks"] += 1
                conn.commit()
            except Exception as e:
                conn.rollback()
                print(f"  [{profile}] insert error {rel_path} chunk {ci}: {e}")
                stats["errors"] += 1
                continue

        if (fi + 1) % 50 == 0 or fi + 1 == len(md_files):
            total_done = stats["chunks"] + stats["skipped"]
            print(
                f"  [{profile}] {fi + 1}/{len(md_files)} files, "
                f"{total_done} chunks ({stats['skipped']} skipped)"
            )

    return stats


def main():
    p = argparse.ArgumentParser(description="Ingest vault markdown into chitta_astrobench")
    p.add_argument("--db", default=DEFAULT_DB_URL, help="Postgres connection URL")
    p.add_argument("--vault", default=str(Path.home() / "vault"), help="Vault root directory")
    p.add_argument("--model-dir", default=None,
                   help="Model directory (auto-selected per embedder if omitted)")
    p.add_argument("--embedder", choices=["bge", "qwen"], default="bge",
                   help="Which embedder to use (default: bge)")
    p.add_argument("--chunk-size", type=int, default=512)
    p.add_argument("--chunk-overlap", type=int, default=64)
    p.add_argument("--profiles", nargs="+", default=list(PROFILES.keys()),
                   help="Which profiles to ingest (default: all)")
    p.add_argument("--sparse-threshold", type=float, default=0.01)
    args = p.parse_args()

    vault = Path(args.vault)
    if not vault.is_dir():
        print(f"Error: vault directory not found: {vault}", file=sys.stderr)
        sys.exit(1)

    print("=== astrobench ingest ===")
    print(f"  db: {args.db}")
    print(f"  vault: {vault}")
    print(f"  embedder: {args.embedder}")
    print(f"  chunk: {args.chunk_size} tokens, {args.chunk_overlap} overlap")
    print(f"  profiles: {args.profiles}")

    if args.embedder == "qwen":
        from embedder_qwen import Embedder as QwenEmbedder, DEFAULT_MODEL_DIR as QWEN_DEFAULT
        model_dir = Path(args.model_dir) if args.model_dir else QWEN_DEFAULT
        print(f"\nLoading Qwen3-VL-Embedding-2B embedder from {model_dir} ...")
        t0 = time.perf_counter()
        embedder = QwenEmbedder(model_dir=model_dir)
    else:
        BGEEmbedder = _load_bge_embedder()
        from embedder import DEFAULT_MODEL_DIR as BGE_DEFAULT
        model_dir = Path(args.model_dir) if args.model_dir else BGE_DEFAULT
        print(f"\nLoading BGE-M3 embedder from {model_dir} ...")
        t0 = time.perf_counter()
        embedder = BGEEmbedder(
            model_dir=model_dir,
            sparse_threshold=args.sparse_threshold,
        )
    print(f"  loaded in {time.perf_counter() - t0:.1f}s")

    conn = psycopg.connect(args.db, autocommit=False)
    register_vector(conn)

    all_stats = {}
    for profile_key in args.profiles:
        if profile_key not in PROFILES:
            print(f"  unknown profile: {profile_key}, skipping")
            continue

        profile_name = PROFILES[profile_key]
        profile_dir = vault / profile_key
        if not profile_dir.is_dir():
            print(f"  [{profile_key}] directory not found: {profile_dir}, skipping")
            continue

        print(f"\n--- {profile_key} ({profile_dir}) ---")
        t0 = time.perf_counter()
        stats = ingest_profile(
            conn, embedder, profile_name, profile_dir,
            args.chunk_size, args.chunk_overlap,
        )
        elapsed = time.perf_counter() - t0
        all_stats[profile_key] = {**stats, "elapsed_s": round(elapsed, 1)}
        print(
            f"  [{profile_key}] done: {stats['files']} files, "
            f"{stats['chunks']} new chunks, {stats['skipped']} skipped, "
            f"{stats['errors']} errors, {elapsed:.1f}s"
        )

    conn.close()

    print("\n=== summary ===")
    total_chunks = sum(s["chunks"] for s in all_stats.values())
    total_skipped = sum(s["skipped"] for s in all_stats.values())
    total_errors = sum(s["errors"] for s in all_stats.values())
    for k, s in all_stats.items():
        print(f"  {k:8s}  files={s['files']:4d}  chunks={s['chunks']:5d}  "
              f"skip={s['skipped']:5d}  err={s['errors']:3d}  {s['elapsed_s']}s")
    print(f"  {'total':8s}  chunks={total_chunks:5d}  skip={total_skipped:5d}  err={total_errors:3d}")


if __name__ == "__main__":
    main()
