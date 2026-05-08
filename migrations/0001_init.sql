-- chitta v0 schema — working model of Josh.
-- Single migration for a clean DB break. See docs/plans/working-model-prd.md.

create extension if not exists vector;

create table memories (
    id                    uuid         primary key,
    profile               text         not null,
    content               text         not null,
    embedding             vector(1024),
    sparse_embedding      jsonb,
    event_time            timestamptz  not null,
    record_time           timestamptz  not null default now(),
    idempotency_key       text         not null,
    source                text,
    memory_type           text         not null
        check (memory_type in (
            'observation', 'episode', 'decision',
            'trait', 'value', 'pattern', 'preference', 'mental_model'
        )),
    tags                  text[]       not null default '{}',
    external_refs         jsonb,
    metadata              jsonb,
    applies_to_domains    text[]       not null default '{}',
    applies_to_skills     text[]       not null default '{}',
    applies_to_projects   text[]       not null default '{}',
    applies_to_situations text[]       not null default '{}',
    superseded_by         uuid         references memories(id),
    confidence            real,
    reinforcement_count   int          not null default 0,
    last_reinforced_at    timestamptz,
    invalidated_at        timestamptz
);

-- Dense ANN (cosine).
create index memories_embedding_idx
    on memories using hnsw (embedding vector_cosine_ops)
    with (m = 16, ef_construction = 64);

-- Per-facet GIN indexes.
create index memories_tags_idx on memories using gin (tags);
create index memories_external_refs_idx on memories using gin (external_refs);
create index memories_applies_to_domains_idx on memories using gin (applies_to_domains);
create index memories_applies_to_skills_idx on memories using gin (applies_to_skills);
create index memories_applies_to_projects_idx on memories using gin (applies_to_projects);
create index memories_applies_to_situations_idx on memories using gin (applies_to_situations);

-- Idempotency: one active key per profile.
create unique index memories_profile_idempotency_key_uniq
    on memories (profile, idempotency_key)
    where invalidated_at is null;

-- Profile-scoped recent-first listing, filtered to active + type.
create index memories_profile_type_record_time_idx
    on memories (profile, memory_type, record_time desc)
    where invalidated_at is null;

-- Tier-0 candidate set: active consolidated rows ordered by confidence.
create index memories_tier0_idx
    on memories (profile, memory_type, confidence desc)
    where memory_type in ('trait', 'value', 'preference', 'pattern', 'mental_model')
      and superseded_by is null
      and invalidated_at is null;

-- ── derivations ─────────────────────────────────────────────────────

create table derivations (
    id              uuid        primary key default gen_random_uuid(),
    derived_id      uuid        not null references memories(id),
    source_id       uuid        not null references memories(id),
    derivation_type text        not null,
    created_at      timestamptz not null default now()
);

create index derivations_derived_id_idx on derivations (derived_id);
create index derivations_source_id_idx on derivations (source_id);
create index derivations_type_idx on derivations (derivation_type);

-- ── query_log (replay / regression eval) ────────────────────────────

create table query_log (
    id              bigserial    primary key,
    profile         text         not null,
    query_text      text         not null,
    embedding       vector(1024) not null,
    k               int          not null,
    min_similarity  real         not null,
    tags            text[]       not null default '{}',
    memory_types    text[]       not null default '{}',
    result_ids      uuid[]       not null default '{}',
    result_scores   real[]       not null default '{}',
    total_available bigint,
    truncated       boolean      not null default false,
    latency_ms      int          not null,
    created_at      timestamptz  not null default now()
);
