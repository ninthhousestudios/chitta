-- Distinguish status-only reflect runs from synthesis runs.
-- status: reflect_status tool counted rows (no synthesis performed)
-- synthesis: chitta reflect ran the full pipeline
alter table reflect_runs add column run_type text;

create index reflect_runs_profile_synthesis_idx
    on reflect_runs (profile, started_at desc)
    where run_type = 'synthesis';
