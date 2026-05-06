#!/usr/bin/env bash
set -euo pipefail

DB_NAME="${1:-chitta_astrobench_qwen}"
DB_USER="${PGUSER:-josh}"

echo "=== astrobench qwen DB provisioning ==="
echo "  database: $DB_NAME"

if psql -lqt | cut -d\| -f1 | grep -qw "$DB_NAME"; then
    echo "  database already exists — skipping createdb"
else
    createdb "$DB_NAME"
    echo "  created database $DB_NAME"
fi

psql -q "$DB_NAME" <<'SQL'
create extension if not exists vector;

create table if not exists memories (
    id                uuid         primary key,
    profile           text         not null,
    content           text         not null,
    embedding         vector(2048) not null,
    event_time        timestamptz  not null,
    record_time       timestamptz  not null default now(),
    tags              text[]       not null default '{}',
    idempotency_key   text         not null,
    source            text,
    metadata          jsonb,
    content_tsvector  tsvector generated always as (to_tsvector('english', content)) stored,
    sparse_embedding  jsonb,
    memory_type       text         not null default 'memory'
);

-- No HNSW index: pgvector limits HNSW to 2000 dims, and exact scan
-- is fine for the ~1400 rows in astrobench.

create index if not exists memories_profile_record_time_idx
    on memories (profile, record_time desc);

create index if not exists memories_tags_idx
    on memories using gin (tags);

create unique index if not exists memories_profile_idempotency_key_uniq
    on memories (profile, idempotency_key);

create index if not exists idx_memories_content_tsvector
    on memories using gin (content_tsvector);

create index if not exists idx_memories_profile_type_record
    on memories (profile, memory_type, record_time desc);

create index if not exists idx_memories_type
    on memories (memory_type);
SQL

echo "=== done — $DB_NAME is ready ==="
