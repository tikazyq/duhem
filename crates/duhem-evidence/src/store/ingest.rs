//! Validated replication of immutable runs into the normal evidence projections.
use std::collections::BTreeSet;

use base64::Engine as _;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{StoreError, is_valid_sha256_hex};
use crate::{EventPayload, RunBundle, RunLineage, RunMeta, RunScope, SqliteStore, Trace};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("invalid bundle: {0}")]
    Invalid(String),
    #[error("conflicting run id")]
    Conflict,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

fn invalid(message: impl ToString) -> IngestError {
    IngestError::Invalid(message.to_string())
}

impl SqliteStore {
    /// Validate first; then insert the receipt, blobs, events and all derived
    /// rows in one transaction. Returns false for an identical resend.
    pub async fn ingest_bundle(&self, bundle: &RunBundle, hash: &str) -> Result<bool, IngestError> {
        let blobs = validate(bundle, hash)?;
        let run = &bundle.run;
        let (parent_run_id, origin) = match &bundle.events[0].payload {
            EventPayload::RunStarted {
                parent_run_id,
                origin,
                ..
            } => (parent_run_id.clone(), *origin),
            _ => unreachable!("validated run_started"),
        };
        let meta = RunMeta {
            run_id: run.run_id.clone(),
            verification: run.verification.clone(),
            schema_version: run.schema_version.clone(),
            inputs: run.inputs.clone(),
            started_at: run.started_at.parse().map_err(invalid)?,
            lineage: RunLineage {
                parent_run_id,
                origin,
            },
            scope: RunScope {
                project_id: run.project_id.clone(),
                verifier_repo: run.verifier_repo.clone(),
                verifier_sha: run.verifier_sha.clone(),
                target_repo: run.target_repo.clone(),
                target_sha: run.target_sha.clone(),
            },
        };
        let mut tx = self.pool().begin().await?;
        // Acquire the SQLite write lock before checking receipts. Concurrent
        // resends then observe the committed receipt rather than racing inserts.
        sqlx::query(
            "INSERT OR IGNORE INTO workspaces (workspace_id, name) VALUES ('local', 'local')",
        )
        .execute(&mut *tx)
        .await?;
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM ingest_receipts WHERE content_hash = ?)",
        )
        .bind(hash)
        .fetch_one(&mut *tx)
        .await?;
        if seen {
            return Ok(false);
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runs WHERE run_id = ?)")
            .bind(&run.run_id)
            .fetch_one(&mut *tx)
            .await?;
        if exists {
            // Each shard creates the same logical parent locally, at a different
            // time. Its one-event, unjudged placeholder has no execution facts.
            let prior: Vec<String> =
                sqlx::query_scalar("SELECT payload FROM events WHERE run_id = ? ORDER BY seq")
                    .bind(&run.run_id)
                    .fetch_all(&mut *tx)
                    .await?;
            let logical = bundle.events.len() == 1
                && run.parent_run_id.is_none()
                && run.verdict.is_none()
                && run.finished_at.is_none();
            let same = prior.len() == 1
                && serde_json::from_str::<crate::Event>(&prior[0])
                    .is_ok_and(|event| event.payload == bundle.events[0].payload);
            let header: (String, String, String) = sqlx::query_as(
                "SELECT verification, schema_version, inputs FROM runs WHERE run_id = ?",
            )
            .bind(&run.run_id)
            .fetch_one(&mut *tx)
            .await?;
            if !logical
                || !same
                || header.0 != run.verification
                || header.1 != run.schema_version
                || serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(
                    &header.2,
                )
                .map_err(invalid)?
                    != run.inputs
            {
                return Err(IngestError::Conflict);
            }
        } else {
            Self::begin_run_tx(&mut tx, &meta).await?;
            for (sha, bytes) in blobs {
                sqlx::query(
                    "INSERT OR IGNORE INTO artifacts (sha256, size, bytes) VALUES (?, ?, ?)",
                )
                .bind(sha)
                .bind(bytes.len() as i64)
                .bind(bytes)
                .execute(&mut *tx)
                .await?;
            }
            for event in &bundle.events {
                Self::append_event_tx(&mut tx, &run.run_id, event).await?;
            }
        }
        sqlx::query("INSERT INTO ingest_receipts (content_hash, run_id) VALUES (?, ?)")
            .bind(hash)
            .bind(&run.run_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(!exists)
    }
}

fn validate(bundle: &RunBundle, hash: &str) -> Result<Vec<(String, Vec<u8>)>, IngestError> {
    if bundle.bundle_version != crate::BUNDLE_VERSION
        || !is_valid_sha256_hex(hash)
        || bundle.content_hash().map_err(invalid)? != hash
    {
        return Err(invalid("version or content hash mismatch"));
    }
    let run = &bundle.run;
    if run.run_id.is_empty()
        || run.schema_version != crate::SCHEMA_VERSION
        || run
            .parent_run_id
            .as_ref()
            .is_some_and(|id| id == &run.run_id || id.is_empty())
    {
        return Err(invalid("invalid run identity or schema version"));
    }
    if run
        .shard
        .as_ref()
        .is_some_and(|shard| shard.index == 0 || shard.total == 0 || shard.index > shard.total)
        || run
            .manifest_digest
            .as_ref()
            .is_some_and(|digest| !is_valid_sha256_hex(digest))
        || run.expected_partition.as_ref().is_some_and(|partition| {
            partition.units.len() > partition.total_units
                || partition.units.iter().collect::<BTreeSet<_>>().len() != partition.units.len()
        })
    {
        return Err(invalid("invalid distributed plan"));
    }
    let trace = Trace::from_events(bundle.events.clone()).map_err(invalid)?;
    let Some(first) = bundle.events.first() else {
        return Err(invalid("missing run_started"));
    };
    match &first.payload {
        EventPayload::RunStarted {
            verification_path,
            inputs,
            schema_version,
            parent_run_id,
            origin,
            ..
        } if verification_path == &run.verification
            && inputs == &run.inputs
            && schema_version == &run.schema_version
            && run
                .parent_run_id
                .as_ref()
                .is_none_or(|id| parent_run_id.as_ref() == Some(id))
            && run.origin.is_none_or(|value| *origin == Some(value))
            && parent_run_id
                .as_ref()
                .is_none_or(|id| !id.is_empty() && id != &run.run_id) => {}
        _ => return Err(invalid("run header disagrees with run_started")),
    }
    let started: chrono::DateTime<chrono::Utc> = run.started_at.parse().map_err(invalid)?;
    if started > first.ts {
        return Err(invalid("start timestamp precedes header"));
    }
    let mut terminal = false;
    for (index, event) in bundle.events.iter().enumerate() {
        if index > 0 && matches!(event.payload, EventPayload::RunStarted { .. }) {
            return Err(invalid("duplicate run_started"));
        }
        if terminal {
            return Err(invalid("events after terminal"));
        }
        terminal = matches!(
            event.payload,
            EventPayload::RunFinished { .. } | EventPayload::RunAborted { .. }
        );
    }
    let last = bundle.events.last().expect("nonempty events");
    if terminal {
        let finished: chrono::DateTime<chrono::Utc> = run
            .finished_at
            .as_ref()
            .ok_or_else(|| invalid("missing finish timestamp"))?
            .parse()
            .map_err(invalid)?;
        let verdict = match last.payload {
            EventPayload::RunFinished { verdict, .. } => verdict,
            _ => None,
        };
        if finished != last.ts
            || run.verdict != verdict
            || run.duration_ms != Some((last.ts - started).num_milliseconds().max(0) as u64)
        {
            return Err(invalid("terminal header mismatch"));
        }
        if let Some(verdict) = run.verdict
            && crate::replay(&trace).map_err(invalid)?.run.state != verdict
        {
            return Err(invalid("replayed verdict mismatch"));
        }
    } else if bundle.events.len() != 1
        || matches!(
            &first.payload,
            EventPayload::RunStarted {
                parent_run_id: Some(_),
                ..
            }
        )
        || run.parent_run_id.is_some()
        || run.verdict.is_some()
        || run.finished_at.is_some()
        || run.duration_ms.is_some()
    {
        return Err(invalid("only logical parents may be unfinished"));
    }
    let distributed = crate::claim_scope::claim_scope(&bundle.events)
        .map(|scope| scope.distributed)
        .unwrap_or_default();
    if run
        .shard
        .as_ref()
        .is_some_and(|value| Some(value) != distributed.shard.as_ref())
        || run
            .manifest_digest
            .as_ref()
            .is_some_and(|value| Some(value) != distributed.manifest_digest.as_ref())
        || run
            .expected_partition
            .as_ref()
            .is_some_and(|value| Some(value) != distributed.expected_partition.as_ref())
    {
        return Err(invalid("distributed header disagrees with events"));
    }
    let expected: BTreeSet<_> = crate::bundle::referenced_blobs(&bundle.events)
        .into_iter()
        .collect();
    let mut supplied = BTreeSet::new();
    let mut blobs = Vec::new();
    for artifact in &bundle.artifacts {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&artifact.bytes_base64)
            .map_err(invalid)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if artifact.sha256 != digest || !supplied.insert(digest.clone()) {
            return Err(invalid("invalid or duplicate artifact"));
        }
        blobs.push((digest, bytes));
    }
    if supplied != expected {
        return Err(invalid("artifact references do not match supplied blobs"));
    }
    Ok(blobs)
}
