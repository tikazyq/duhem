-- Immutable replication receipts commit with the run and its projections.
CREATE TABLE ingest_receipts (
    content_hash TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(run_id),
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER ingest_receipts_no_update BEFORE UPDATE ON ingest_receipts
BEGIN SELECT RAISE(ABORT, 'ingest receipts are append-only'); END;
CREATE TRIGGER ingest_receipts_no_delete BEFORE DELETE ON ingest_receipts
BEGIN SELECT RAISE(ABORT, 'ingest receipts are append-only'); END;
