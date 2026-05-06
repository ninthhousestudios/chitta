-- Migration 0009: derivations table.
-- Tracks lineage when a memory is synthesized from source memories
-- (e.g. /reflect consolidating observations into a mental model).

CREATE TABLE derivations (
    id              uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    derived_id      uuid        NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    source_id       uuid        NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    derivation_type text        NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT derivations_no_self CHECK (derived_id != source_id)
);

CREATE INDEX derivations_derived_idx ON derivations (derived_id);
CREATE INDEX derivations_source_idx  ON derivations (source_id);
CREATE INDEX derivations_type_idx    ON derivations (derivation_type);
