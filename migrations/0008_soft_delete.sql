-- Migration 0008: soft-delete via invalidated_at.
-- Non-null means the memory is logically deleted. All read paths must
-- filter on invalidated_at IS NULL.

ALTER TABLE memories ADD COLUMN invalidated_at timestamptz;

-- Partial index: active memories only. Replaces the original
-- profile+record_time index for the common read path.
CREATE INDEX memories_active_profile_record_time_idx
    ON memories (profile, record_time DESC)
    WHERE invalidated_at IS NULL;

-- Partial index on memory_type for active rows.
CREATE INDEX memories_active_profile_type_record_idx
    ON memories (profile, memory_type, record_time DESC)
    WHERE invalidated_at IS NULL;
