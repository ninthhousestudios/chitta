-- Rename tier-0 index to match PRD §Schema naming convention.
ALTER INDEX memories_tier0_idx RENAME TO memories_consolidated_active_idx;
