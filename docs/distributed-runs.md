# Distributed runs

Each CI machine runs the same `duhem run` with its own environment. Duhem
partitions selected criteria and native runs locally; CI still starts the
machines and gates their results. Each machine must have its own mutable test
environment. Automatic delivery is available; server aggregation is a later slice.

## Automatic delivery to a dashboard

Start an ingest-enabled OSS dashboard and configure each runner with its exact
ingest URL. The same configuration works for ordinary runs:

```sh
DUHEM_HUB_TOKEN=example-token duhem dashboard --ingest --db hub.db --host 127.0.0.1 --port 7878
export DUHEM_HUB_URL=http://127.0.0.1:7878/api/ingest
export DUHEM_HUB_TOKEN=example-token
duhem run verifications/sharding-example --shard 1/2 --run-id example-invocation
```

The token is optional on loopback, but a non-loopback bind requires a non-empty
`DUHEM_HUB_TOKEN`. Use HTTPS through your own proxy when sending tokens across
networks. The flag enables writes only on `POST /api/ingest`; browsing remains
read-only. Without the flag the endpoint does not exist. With no URL configured,
execution and storage remain entirely local.

After execution, Duhem sends the invocation's evidence tree parent-first. Each
run is a separate v1 bundle with its own content hash. Ordinary manifest trees
are `suite → leaf → nested invocation`; sharded manifest trees are
`logical parent → shard suite → leaf → nested invocation`. Single-leaf shards
omit the suite layer. Nested invocations defer shipping to their enclosing
invocation, so a child never arrives before its parent. Only this invocation's
descendants and required ancestors are sent, never unrelated runs in the store.
Native report and parsed-result blobs are included alongside stdout/stderr.

Each bundle gets one attempt, a 10 s request timeout, and the entire automatic
tree gets at most 30 s including assembly. Delivery stops on its first failure,
warns once on stderr, and leaves the verdict, stdout report and exit code intact.
The receiver caps each envelope at 16 MiB. There is no splitting, retry queue or
background delivery; `duhem ship <run-id> --db <path>` can resend a bundle.
Identical resends are no-ops. Ingest validates headers, events and artifact
references, replays judged streams, and commits receipts with all normal SQLite
projections atomically.

Logical parents from separate shards are accepted idempotently and stay
unjudged. This slice does not aggregate shard verdicts, close logical parents,
detect missing partitions, reconcile manifest digests across shards, publish
combined statuses, or provide a queue. CI jobs still gate their own results.
See the [bundle contract](run-bundle-contract.md) and the
[worked ingest verification](../verifications/ingest-example/duhem.yml).

## Detection and overrides

`--shard k/n` overrides `DUHEM_SHARD`, which overrides native CI detection.
Indexes in flags and `DUHEM_SHARD` are **one-based**, like Playwright.
`--run-id` overrides `DUHEM_RUN_ID`, which overrides the shared CI identity.
A flag or generic shard value can use the detected CI identity. Outside CI,
provide a shared identity explicitly. Invalid shard values fail before execution.
Without shard information, ordinary run behavior and JSON remain unchanged.

| Source | Index / total | Native index | Shared identity components |
| --- | --- | --- | --- |
| [GitLab](https://docs.gitlab.com/ci/variables/predefined_variables/) | `CI_NODE_INDEX` / `CI_NODE_TOTAL` | one-based | `CI_PIPELINE_ID`, `CI_JOB_NAME` without its trailing ` k/n` |
| [Buildkite](https://buildkite.com/docs/pipelines/configure/environment-variables) | `BUILDKITE_PARALLEL_JOB` / `BUILDKITE_PARALLEL_JOB_COUNT` | zero-based | `BUILDKITE_BUILD_ID`, `BUILDKITE_STEP_ID` |
| [CircleCI](https://circleci.com/docs/reference/variables/) | `CIRCLE_NODE_INDEX` / `CIRCLE_NODE_TOTAL` | zero-based | `CIRCLE_WORKFLOW_ID`, `CIRCLE_JOB` |
| [GitHub Actions](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#strategy-context) | Map `strategy.job-index + 1` and `strategy.job-total` to `DUHEM_SHARD` | context index is zero-based | `GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`, `GITHUB_JOB` |

GitHub Actions has no native shard-index environment variable. Its
[default variables](https://docs.github.com/en/actions/reference/variables-reference)
provide the shared identity only when sharding was selected. GitLab's total
alone does not enable sharding: it is also set for nonparallel jobs.
See [GitLab parallel jobs](https://docs.gitlab.com/ci/yaml/#parallel) for the
`job 1/N` naming convention and the [Playwright GitLab example](https://playwright.dev/docs/ci#gitlab-ci)
that passes `CI_NODE_INDEX` directly to its one-based `--shard` flag.
CI workflow recipes will follow separately.
Use a new shared identity for each logical invocation; if a provider's retry
keeps these components unchanged, include your attempt identifier explicitly.

## Partition and lifecycle

Selection (`--filter` and directory selection) happens before partitioning.
Each selected `(leaf, criterion)` is one indivisible unit, even if its filter
selects only some checks. Root-manifest and leaf-level native runs are units too.
Unit IDs are JSON-encoded triples `[kind, relative-leaf-path, authored-id]`;
`$manifest` identifies suite-native runs and `$leaf` a single-leaf invocation.
Duhem sorts these IDs lexically and assigns position `i` to shard
`(i mod n) + 1`. Round-robin keeps unit counts within one; it does not estimate
duration. The same definitions, selection and `n` produce disjoint partitions
covering the selection, without coordination or machine-dependent hashes.

A shard runs only the leaves it needs. Provisioning, leaf setup, teardown and
cleanup run on each shard that owns a unit in that leaf. Checks of one criterion
never cross machines. A shard with no assigned units succeeds when the overall
selection is nonempty; an empty user selection retains its inconclusive verdict.
`--dry-run` shows the narrowed plan without opening the evidence store.

## Evidence and identity

`--run-id` is allowed on manifests when sharding. Its value names the logical
parent, rather than pinning a shard's own record. Duhem hashes this shared
identity with a versioned namespace into a valid ULID; each shard gets a fresh
ULID. A local unjudged logical-parent row satisfies SQLite's parent foreign key.
It is intentionally left unfinished, because one machine cannot judge all
shards. Manifest leaf records remain children of their shard's suite record.

Shard JSON summaries add these optional fields, also persisted in the
`claim_scope_declared` evidence event:

```json
{
  "origin": "shard",
  "parent_run_id": "<logical-parent-ULID>",
  "shard": {"index": 1, "total": 2, "source": "flag"},
  "manifest_digest": "<SHA-256>",
  "expected_partition": {
    "units": ["[\"criterion\",\"feature/duhem.yml\",\"AC-1\"]"],
    "total_units": 8
  }
}
```

The digest covers the parsed manifest and loaded leaf definitions before
selection and input resolution, including composed definitions. It excludes
absolute source paths and resolved runtime inputs. The expected partition lists
this shard's unit IDs, the total selected unit count, and other selected units
in `excluded: [{unit, reason: "shard"}]` (including native units). Check exclusions carry
`reason: shard`; user-filter exclusions retain `reason: check_filter`. Native
exclusions remain available in `excluded_native_runs`. Ordinary summaries omit
all distributed fields. On manifest leaves the plan describes their enclosing
shard; the actual store lineage still points to the local suite record.

The [worked definition](../verifications/sharding-example/duhem.yml) can be run
on two machines with the same identity:

```sh
duhem run verifications/sharding-example --shard 1/2 --run-id example-invocation --reporter json
duhem run verifications/sharding-example --shard 2/2 --run-id example-invocation --reporter json
```
