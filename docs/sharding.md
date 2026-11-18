# Distributed runs

Run the same selection on each machine with a one-based shard index:

```sh
duhem run --shard 1/3 --run-id build-123
duhem run --shard 2/3 --run-id build-123
duhem run --shard 3/3 --run-id build-123
```

Each selected criterion is one unit, keeping its selected checks and criterion
setup together. Each selected native command is another unit. Duhem sorts units
by manifest-relative verification path, kind and ID, then assigns them round-robin.
Unit counts differ by at most one; runtime duration is not predicted. Directory
and `--filter` selection apply before partitioning. The same flags also partition
a single leaf's criteria. Every machine must use the same definitions, selection,
profile and shard count. `--dry-run` prints only that shard's assigned work and
creates no evidence store.

`--shard` overrides `DUHEM_SHARD=k/n`, which overrides automatic CI detection:

| Provider | Index | Count | Shared identity |
| --- | --- | --- | --- |
| GitLab | `CI_NODE_INDEX` (one-based) | `CI_NODE_TOTAL` | `CI_PIPELINE_ID` + `CI_JOB_NAME` without its ` k/n` suffix |
| Buildkite | `BUILDKITE_PARALLEL_JOB` (zero-based) | `BUILDKITE_PARALLEL_JOB_COUNT` | `BUILDKITE_BUILD_ID` + `BUILDKITE_STEP_ID` |
| CircleCI | `CIRCLE_NODE_INDEX` (zero-based) | `CIRCLE_NODE_TOTAL` | `CIRCLE_WORKFLOW_ID` + `CIRCLE_JOB` |

`--run-id` overrides `DUHEM_RUN_ID`. With sharding it names the logical parent;
each local shard container and leaf still gets a fresh execution ID. Without an
explicit parent ID, Duhem hashes the plan and detected CI identity. Outside CI,
the same plan produces the same logical parent; use an explicit shared ID to
distinguish repeated invocations. Run identity variables alone do not enable
sharding or change an unsharded invocation.

For GitHub Actions, map the strategy contexts explicitly:

```yaml
strategy:
  matrix:
    shard: [1, 2, 3]
steps:
  - run: duhem run
    env:
      DUHEM_SHARD: ${{ matrix.shard }}/${{ strategy.job-total }}
      DUHEM_RUN_ID: ${{ github.run_id }}-${{ github.run_attempt }}-verify
```

Evidence records `origin: shard`, the logical parent, source, one-based index,
count, manifest SHA-256 digest, plan SHA-256 digest, and the full expected
partition in the shard container's `run_started` event and SQLite `run_shards`
projection. Leaf runs remain suite children of the local shard container. An
empty selection still records a plan; shards with no assigned work start no
lifecycle commands. Invalid shard values fail before opening the evidence store.
With no shard settings, existing summaries and dry-run output remain unchanged.

CI owns orchestration and the combined gate: require every shard to succeed.
This feature does not schedule machines or aggregate remote evidence. See the
[worked example](../verifications/sharding-example/duhem.yml).
