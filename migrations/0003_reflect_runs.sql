-- Operational metadata for /reflect runs. Not working-model content.
create table reflect_runs (
    id           uuid        primary key default gen_random_uuid(),
    profile      text        not null,
    started_at   timestamptz not null default now(),
    completed_at timestamptz,
    rows_scanned int         not null default 0,
    summary      jsonb
);

create index reflect_runs_profile_completed_idx
    on reflect_runs (profile, completed_at desc)
    where completed_at is not null;
