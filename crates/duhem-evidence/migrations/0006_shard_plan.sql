-- A logical parent may not exist in this machine's store. Keep the
-- original foreign key for local suite and invocation lineage.
ALTER TABLE runs ADD COLUMN shard_parent_run_id TEXT;
ALTER TABLE runs ADD COLUMN shard_plan TEXT CHECK (
    (shard_parent_run_id IS NULL AND shard_plan IS NULL) OR
    (shard_parent_run_id IS NOT NULL AND shard_plan IS NOT NULL AND
     parent_run_id IS NULL AND origin IS NULL AND json_valid(shard_plan))
);
