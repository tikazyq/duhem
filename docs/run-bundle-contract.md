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
    // verdict present once judged, timing present once terminal:
    "verdict": "pass",                  // pass | fail | inconclusive:<cause>
    "finished_at": "…", "duration_ms": 1000,
    // #190 scope + provenance (all optional):
    "project_id": "github.com/acme/app",
    "verifier_repo": "github.com/onsager-ai/duhem", "verifier_sha": "…",
    "target_repo": "github.com/acme/app", "target_sha": "…",
    // Optional lineage and distributed plan fields:
    "parent_run_id": "01…", "origin": "shard",  // suite | invocation | shard
    "shard": {"index": 1, "total": 2, "source": "flag"},
    "manifest_digest": "…64 lowercase hex…",
    "expected_partition": {"units": ["…"], "total_units": 2,
      "excluded": [{"unit": "…", "reason": "shard"}]}
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

The URL is the full endpoint, including `/api/ingest` for the OSS dashboard.
A 2xx response acknowledges a committed run or an identical resend. The
dashboard verifies the SHA-256 against the received bytes, validates lifecycle
and artifact references, and replays judged traces before inserting the events,
blobs, projections and receipt in a single SQLite transaction. Run IDs are
ASCII letters, digits, hyphens or underscores. A conflicting run ID or missing
parent returns 409. The shared shard parent is an unjudged `run_started`-only
placeholder. Multiple machines can deliver that parent ID with different local
start times and paths. The first accepted parent record is retained.

Automatic delivery sends one v1 envelope per run, in parent-first order, with
its own hash. `parent_run_id` and `origin` describe the immediate parent link.
Shard fields describe the enclosing shard even on a suite leaf. Referenced
native stdout, stderr, reports and parsed results travel with the run. New
traces can record the optional `setup_finished.abort_cause` and claim-scope
preconditions so replay can reconstruct early and native-only outcomes.

The v1 transport is a single JSON envelope (base64 artifacts). The OSS dashboard
limits each request body to 16 MiB, including base64. Runs
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
