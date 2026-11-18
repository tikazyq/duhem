CREATE TABLE ingest_receipts (
    run_id TEXT PRIMARY KEY REFERENCES runs(run_id),
    content_hash TEXT NOT NULL UNIQUE,
    logical INTEGER NOT NULL CHECK (logical IN (0, 1))
);
