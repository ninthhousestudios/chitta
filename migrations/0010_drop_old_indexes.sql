-- Migration 0010: drop pre-soft-delete indexes.
-- Replaced by partial indexes in 0008 that exclude invalidated rows.

DROP INDEX IF EXISTS memories_profile_record_time_idx;
DROP INDEX IF EXISTS idx_memories_profile_type_record;
