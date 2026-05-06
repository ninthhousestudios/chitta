-- Replace the total unique index on (profile, idempotency_key) with a partial
-- index that only covers active (non-deleted) rows. The old index prevented
-- re-storing a memory with the same idempotency key after soft-delete.
drop index if exists memories_profile_idempotency_key_uniq;

create unique index memories_profile_idempotency_key_active_uniq
    on memories (profile, idempotency_key)
    where invalidated_at is null;
