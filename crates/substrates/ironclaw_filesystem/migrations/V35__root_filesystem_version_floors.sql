-- Per-path version high-water marks of deleted records.
--
-- A record's version is its CAS token. Before this table a path's version
-- restarted at 1 when the path was deleted and recreated, so a token captured
-- against the deleted incarnation could match the new one (ABA): a CAS write
-- with it overwrote the new record, a conditional delete removed it. Every
-- delete now keeps the highest version it removed per path here, in the same
-- statement as the delete, and every create starts above it (CONTRACT.md
-- invariant 9). Rows are tiny (path + version) and are never removed: a
-- recreated path keeps its mark so later deletes keep raising it.
--
-- Compatibility: additive. No existing row changes; old binaries ignore the
-- table. Paths deleted before this migration have no mark, so their first
-- recreate after the upgrade may still restart at 1 once — exactly the
-- pre-upgrade behaviour, and only for a token captured before the upgrade.
--
-- Rollback plan: the table can be left in place (old binaries never read it);
-- to remove it, `DROP TABLE IF EXISTS root_filesystem_version_floors;`, which
-- returns recreates to restarting at version 1.

CREATE TABLE IF NOT EXISTS root_filesystem_version_floors (
    path TEXT COLLATE "C" PRIMARY KEY CHECK (path LIKE '/%'),
    version BIGINT NOT NULL CHECK (version > 0)
);
