-- Distributed parents are logical identities shared across independent stores,
-- not foreign keys to a run on this machine. Preserve the historical runs table
-- and its lineage constraints; project shard headers separately, append-only.
CREATE TABLE run_shards (
    run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
    parent_run_id TEXT NOT NULL,
    shard_index INTEGER NOT NULL CHECK (shard_index > 0),
    shard_total INTEGER NOT NULL CHECK (shard_total >= shard_index),
    source TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    plan TEXT NOT NULL
);
CREATE INDEX run_shards_parent ON run_shards(parent_run_id);
CREATE TRIGGER run_shards_no_update BEFORE UPDATE ON run_shards
BEGIN SELECT RAISE(ABORT, 'duhem store is append-only: UPDATE rejected on run_shards'); END;
CREATE TRIGGER run_shards_no_delete BEFORE DELETE ON run_shards
BEGIN SELECT RAISE(ABORT, 'duhem store is append-only: DELETE rejected on run_shards'); END;
