# Automatic ingest example

Run from the repository root with both built binaries available:

```sh
duhem run verifications/ingest-example \
  --inputs duhem_bin="$PWD/target/debug/duhem" \
  --inputs dashboard_bin="$PWD/target/debug/duhem-dashboard"
```

The criterion comes from #663: completed evidence reaches the configured
dashboard unchanged and duplicate delivery does not create another run. Its
check starts a real loopback dashboard with ingest enabled, runs a real passing
fixture into a separate SQLite store with `DUHEM_HUB_URL` configured, reads its
remote run detail, manually resends the bundle and verifies the dashboard still
lists exactly one run. A failed command, missing run or duplicate row makes the
check fail. Temporary stores and the receiver process are cleaned up.

This is self-regression coverage, not independent attestation. The workspace
integration tests add rejected credentials, malformed bundles, the body bound,
projection rollback, sharded/nested trees and delivery failure exit behavior.
See [distributed runs](../../docs/distributed-runs.md) for deployment configuration,
limits and the remaining aggregation scope.
