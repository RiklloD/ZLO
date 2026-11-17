-- Per-path serialization of record writes (CONTRACT.md invariant 9).
--
-- PostgreSQL runs writers concurrently under READ COMMITTED, and a create
-- used to compute its version inside its own statement from that
-- statement's snapshot. Two writers were enough to repeat a version: with
-- v1 committed, transaction A updates the record to v2 (uncommitted); B's
-- create sees v1, chooses 2 and waits on A's unique key; A deletes (floor 2)
-- and commits; B inserts v2 — a token A already handed out.
--
-- Every record create, CAS update, and delete now takes a transaction-scoped
-- advisory lock on its path before it writes, and a create computes its
-- version only after that lock is granted: `root_filesystem_next_record_version`
-- is VOLATILE, so under READ COMMITTED its query takes a fresh snapshot after
-- the lock wait and sees everything the previous lock holder committed. The
-- lock is held to the end of the writer's transaction (an open StorageTxn
-- keeps it through its later statements), never across a commit.
--
-- The key is a 64-bit hash of the full virtual path (the path carries the
-- mount), seeded so it does not share a key space with other
-- `hashtextextended(_, 0)` users. A collision only makes two unrelated paths
-- wait for each other; it never weakens the invariant. Cost: one advisory lock
-- per record write, taken inside the write statement — no extra round trip.
--
-- Compatibility: additive (two functions, no table or row change). Old
-- binaries never call them. Rollback: the functions can stay; to remove them,
-- `DROP FUNCTION IF EXISTS root_filesystem_next_record_version(TEXT),
-- root_filesystem_lock_record_path(TEXT);` after rolling the binary back.

CREATE OR REPLACE FUNCTION root_filesystem_lock_record_path(record_path TEXT)
RETURNS BOOLEAN
LANGUAGE plpgsql
VOLATILE
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended(record_path, 7240371));
    RETURN TRUE;
END;
$$;

CREATE OR REPLACE FUNCTION root_filesystem_next_record_version(record_path TEXT)
RETURNS BIGINT
LANGUAGE plpgsql
VOLATILE
AS $$
DECLARE
    next_version BIGINT;
BEGIN
    PERFORM root_filesystem_lock_record_path(record_path);
    -- A fresh snapshot, taken after the lock wait: the floor and live row
    -- any earlier lock holder committed are visible here.
    SELECT GREATEST(
        COALESCE((SELECT floor.version FROM root_filesystem_version_floors AS floor
                  WHERE floor.path = record_path), 0),
        COALESCE((SELECT live.version FROM root_filesystem_entries AS live
                  WHERE live.path = record_path AND NOT live.is_dir), 0)
    ) + 1
    INTO next_version;
    RETURN next_version;
END;
$$;
