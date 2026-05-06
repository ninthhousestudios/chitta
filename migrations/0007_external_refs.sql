-- Migration 0007: external_refs typed column.
-- Allows memories to link to external artifacts (files, commits, yojana
-- tasks, other memories, sessions, URLs). JSONB array of {kind, ref} objects.

ALTER TABLE memories ADD COLUMN external_refs jsonb;

CREATE INDEX memories_external_refs_idx
    ON memories USING gin (external_refs jsonb_path_ops);
