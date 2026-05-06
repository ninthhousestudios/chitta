#!/usr/bin/env python3
"""Find hapax tokens in the astrobench corpus for slice B authoring.

Scans all chunks in the astro profile, tokenizes them, and finds tokens
that appear in exactly one chunk. Outputs candidates with surrounding
context so Josh can pick meaningful identifiers for slice B queries.

Usage:
    cd bench/astrobench
    uv run python find-unique-tokens.py
    uv run python find-unique-tokens.py --min-length 4 --limit 200
"""

import argparse
import re
from collections import defaultdict
from pathlib import Path

import psycopg

DEFAULT_DB_URL = "postgresql://josh:ogham@localhost/chitta_astrobench"


def find_unique_tokens(
    db_url: str,
    profile: str,
    min_length: int,
    limit: int,
):
    conn = psycopg.connect(db_url)

    with conn.cursor() as cur:
        cur.execute(
            "SELECT id, content, metadata FROM memories "
            "WHERE profile = %s ORDER BY record_time",
            (profile,),
        )
        rows = cur.fetchall()

    conn.close()

    if not rows:
        print(f"No chunks found for profile '{profile}'")
        return

    print(f"Loaded {len(rows)} chunks from profile '{profile}'")

    # Extract "word-like" tokens from each chunk's content.
    # Includes numbers, diacritics, colons (for degree:min:sec), hyphens.
    TOKEN_RE = re.compile(r"[\wĀ-ɏ:.-]+", re.UNICODE)

    token_to_chunks: dict[str, list[tuple[str, str, str]]] = defaultdict(list)

    for chunk_id, content, metadata in rows:
        tokens = TOKEN_RE.findall(content)
        seen_in_chunk = set()
        for tok in tokens:
            tok_lower = tok.lower()
            if tok_lower in seen_in_chunk:
                continue
            seen_in_chunk.add(tok_lower)

            # Find surrounding context (±40 chars around first occurrence)
            idx = content.lower().find(tok_lower)
            if idx >= 0:
                start = max(0, idx - 40)
                end = min(len(content), idx + len(tok) + 40)
                context = content[start:end].replace("\n", " ")
            else:
                context = ""

            source_path = ""
            if metadata and isinstance(metadata, dict):
                source_path = metadata.get("source_path", "")

            token_to_chunks[tok_lower].append((chunk_id, source_path, context))

    # Filter to hapax: tokens appearing in exactly one chunk
    hapax = {
        tok: info[0]
        for tok, info in token_to_chunks.items()
        if len(info) == 1 and len(tok) >= min_length
    }

    # Sort by token length descending (longer tokens are more interesting)
    sorted_hapax = sorted(hapax.items(), key=lambda x: -len(x[0]))

    if limit:
        sorted_hapax = sorted_hapax[:limit]

    print(f"\nFound {len(hapax)} hapax tokens (>= {min_length} chars)")
    print(f"Showing top {len(sorted_hapax)}:\n")
    print(f"{'TOKEN':40s}  {'CHUNK_ID':36s}  {'SOURCE':30s}  CONTEXT")
    print("-" * 160)

    for tok, (chunk_id, source_path, context) in sorted_hapax:
        print(f"{tok:40s}  {str(chunk_id):36s}  {source_path:30s}  ...{context}...")

    # Also write to a file for easier browsing
    out_path = Path("hapax-candidates.txt")
    with open(out_path, "w") as f:
        f.write(f"# Hapax tokens for profile '{profile}' ({len(hapax)} total)\n")
        f.write(f"# Tokens appearing in exactly one chunk, >= {min_length} chars\n")
        f.write(f"# Format: TOKEN | CHUNK_ID | SOURCE | CONTEXT\n\n")
        for tok, (chunk_id, source_path, context) in sorted_hapax:
            f.write(f"{tok} | {str(chunk_id)} | {source_path} | ...{context}...\n")

    print(f"\nFull list written to {out_path}")


def main():
    p = argparse.ArgumentParser(
        description="Find hapax (unique) tokens for slice B authoring"
    )
    p.add_argument("--db", default=DEFAULT_DB_URL)
    p.add_argument("--profile", default="astro")
    p.add_argument("--min-length", type=int, default=4,
                   help="Minimum token character length")
    p.add_argument("--limit", type=int, default=300,
                   help="Max candidates to show (0 = all)")
    args = p.parse_args()

    find_unique_tokens(args.db, args.profile, args.min_length, args.limit)


if __name__ == "__main__":
    main()
