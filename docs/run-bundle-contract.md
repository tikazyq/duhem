# The run-bundle wire contract (#194)

One immutable run, self-contained, versioned — the seam between the
open-source engine and the hosted hub (#188 open-core boundary). The
closed-source hub builds against this document plus the contract test
in `crates/duhem-evidence/tests/bundle_contract.rs`; the test is the
enforcement, this file is the narrative.

## Envelope (`bundle_version: 1`)

The canonical wire form is the compact JSON serialization of
`duhem_evidence::RunBundle`:

```jsonc
{
  "bundle_version": 1,
  "run": {
    "run_id": "01…",                    // ULID
    "verification": "verifications/login/duhem.yml",
    "schema_version": "v1",             // trace wire version (#10)
    "inputs": { … },
    "started_at": "2026-07-06T00:00:00.000Z",
    // present once judged:
    "verdict": "pass",                  // pass | fail | inconclusive:<cause>
    "finished_at": "…", "duration_ms": 1000,
    // #190 scope + provenance (all optional):
    "project_id": "github.com/acme/app",
    "verifier_repo": "github.com/onsager-ai/duhem", "verifier_sha": "…",
    "target_repo": "github.com/acme/app", "target_sha": "…",
    // optional tree/distributed metadata (#663):
    "parent_run_id": "01…", "origin": "shard", // suite | invocation | shard
    "shard": {"index": 1, "total": 2, "source": "flag"},
    "manifest_digest": "…64 hex…",
    "expected_partition": {"units": ["…"], "total_units": 8}
  },
  "events": [ { "seq": 0, "ts": "…", "kind": "…", … } ],   // full #10 stream, seq order
  "artifacts": [ { "sha256": "…64 hex…", "bytes_base64": "…" } ]  // sorted by sha
}
```

Everything the hub needs is inside: replay recomputes the verdict
from `events`; the normalized projections (criteria / checks /
assertions / spans) are derivable server-side exactly as the local
store derives them. Shipping is **replication, not dual-truth**: a
bundle is immutable and identified by content.

## Identity & idempotency

The bundle's **content hash** is the lowercase-hex SHA-256 of the
canonical wire bytes. `duhem ship` sends it as
`X-Duhem-Content-Hash`; a hub that already stores that hash answers
2xx without re-storing. Re-shipping the same run is therefore always
safe.

## Transport

`POST <DUHEM_HUB_URL>` with:

| Header | Value |
|--------|-------|
| `Content-Type` | `application/json` |
| `X-Duhem-Bundle-Version` | `1` |
| `X-Duhem-Content-Hash` | the content hash |
| `Authorization` | `Bearer <DUHEM_HUB_TOKEN>` (when configured) |

2xx = ingested (or already present). Configuring `DUHEM_HUB_URL` enables
automatic delivery after every `duhem run`, including failed verdicts. Each
bundle gets one attempt with a 10 s request timeout; the whole tree has a
30 s deadline. A delivery failure produces one warning and preserves the
verdict and exit code. There is no retry queue. `duhem ship` remains available
for explicit resends.

The OSS dashboard accepts `POST /api/ingest` only with `--ingest`. It limits
bodies to 16 MiB and requires a non-empty `DUHEM_HUB_TOKEN` on non-loopback
binds. The token is optional on loopback; when configured it is required on
every ingest request. Browsing still uses a read-only SQLite connection.
Ingest validates the version/hash headers, run header against events, contiguous
sequences, terminal placement, artifact digests and reference completeness. It
replays judged streams using the deterministic judge. Receipts, blobs, events
and the usual projections commit in one SQLite transaction. Identical hashes
return 200 without writes; conflicting immutable run IDs return 409. Invalid
bundles return 400, oversized bodies 413, and storage failures 500.

A tree travels as **one v1 envelope per run**, ancestors first, each with its
own content hash. Lineage uses the actual store parent, including suite and
nested-invocation parents; distributed plan metadata is copied from claim-scope
events. Native stdout, stderr, source report and normalized result blobs travel
with their event references. A one-event, unjudged logical parent may be sent
before shard children. Equivalent logical parents from different machines are
accepted idempotently despite different creation times; they remain unjudged.
Aggregation is a later slice. See [distributed runs](distributed-runs.md).

The v1 transport is a single JSON envelope (base64 artifacts). Runs
with large artifact sets may outgrow it; a chunked/multipart
transport would be `bundle_version: 2` — tracked as the open
question on #194.

## Two destinations, one format

`duhem export <run-id>` writes the same `RunBundle` as a browsable
directory (`bundle.json` header + `events.jsonl` + `artifacts/…`),
and `RunBundle::from_dir` reads it back byte-equivalently — the
round-trip the contract test pins.

## Change discipline

Any field rename, removal, or semantic change bumps `BUNDLE_VERSION`,
updates the golden in `bundle_contract.rs`, and lands with a
coordinated hub-side change. Additive optional fields are allowed
within a version; the hub must ignore unknown fields.
