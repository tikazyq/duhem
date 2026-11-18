use base64::Engine as _;
use thiserror::Error;

use super::{RunLineage, RunMeta, RunScope, SqliteStore, StoreError, parse_ts};
use crate::bundle::{distributed_plan, referenced_blobs};
use crate::distributed::manifest_digest;
use crate::{BUNDLE_VERSION, EventPayload, RunBundle, RunOrigin, Trace, replay};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("run ID already exists with different evidence")]
    Conflict,
    #[error("parent run must be delivered first")]
    MissingParent,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for IngestError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error.into())
    }
}

impl SqliteStore {
    /// Import a v1 envelope and its projections in one write transaction.
    pub async fn ingest_bundle(&self, bytes: &[u8], hash: &str) -> Result<(), IngestError> {
        if manifest_digest(bytes) != hash {
            return Err(IngestError::Invalid("content hash does not match body"));
        }
        let bundle: RunBundle = serde_json::from_slice(bytes)
            .map_err(|_| IngestError::Invalid("invalid bundle JSON"))?;
        let (meta, trace, logical) = validate_run(&bundle)?;
        let artifacts = validate_artifacts(&bundle)?;

        // Acquire the write lock before checking receipts. Concurrent sends
        // of the same run see the committed result of the first writer.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let receipt: Option<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT i.content_hash, i.logical, r.parent_run_id FROM ingest_receipts i
             JOIN runs r USING (run_id) WHERE i.run_id = ?",
        )
        .bind(&meta.run_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((stored_hash, stored_logical, stored_parent)) = receipt {
            return if stored_hash == hash
                || (stored_logical && logical && stored_parent == meta.lineage.parent_run_id)
            {
                Ok(())
            } else {
                Err(IngestError::Conflict)
            };
        }
        let exists = |id: String| {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM runs WHERE run_id = ?)")
                .bind(id)
        };
        if exists(meta.run_id.clone()).fetch_one(&mut *tx).await? {
            return Err(IngestError::Conflict);
        }
        if let Some(parent) = &meta.lineage.parent_run_id
            && !exists(parent.clone()).fetch_one(&mut *tx).await?
        {
            return Err(IngestError::MissingParent);
        }
        Self::insert_run(&mut tx, &meta).await?;
        for (sha, bytes) in artifacts {
            sqlx::query("INSERT OR IGNORE INTO artifacts (sha256, size, bytes) VALUES (?, ?, ?)")
                .bind(sha)
                .bind(bytes.len() as i64)
                .bind(bytes)
                .execute(&mut *tx)
                .await?;
        }
        for event in trace.events() {
            Self::insert_event(&mut tx, &meta.run_id, event).await?;
        }
        sqlx::query("INSERT INTO ingest_receipts (run_id, content_hash, logical) VALUES (?, ?, ?)")
            .bind(&meta.run_id)
            .bind(hash)
            .bind(logical)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

fn validate_run(bundle: &RunBundle) -> Result<(RunMeta, Trace, bool), IngestError> {
    let run = &bundle.run;
    let bad = IngestError::Invalid("invalid run header or lifecycle");
    if bundle.bundle_version != BUNDLE_VERSION
        || run.run_id.is_empty()
        || !run
            .run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        || run.verification.trim().is_empty()
        || run.schema_version != crate::SCHEMA_VERSION
    {
        return Err(bad);
    }
    let trace = Trace::from_events(bundle.events.clone())
        .map_err(|_| IngestError::Invalid("invalid event sequence"))?;
    let Some(first) = trace.events().first() else {
        return Err(bad);
    };
    let EventPayload::RunStarted {
        verification_path,
        inputs,
        schema_version,
        parent_run_id,
        origin,
        definition,
        ..
    } = &first.payload
    else {
        return Err(bad);
    };
    if verification_path != &run.verification
        || inputs != &run.inputs
        || schema_version != &run.schema_version
        || run
            .parent_run_id
            .as_ref()
            .is_some_and(|id| Some(id) != parent_run_id.as_ref())
        || run.origin.is_some_and(|value| Some(value) != *origin)
        || parent_run_id.is_some() != origin.is_some()
        || parent_run_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id == &run.run_id)
    {
        return Err(bad);
    }
    let started_at =
        parse_ts(&run.started_at).map_err(|_| IngestError::Invalid("invalid start time"))?;
    let plan = distributed_plan(trace.events());
    if run.shard.as_ref().is_some_and(|shard| {
        shard.index == 0
            || shard.index > shard.total
            || shard.source.is_empty()
            || plan.and_then(|p| p.shard.as_ref()) != Some(shard)
    }) || run.manifest_digest.as_ref().is_some_and(|digest| {
        !super::is_valid_sha256_hex(digest)
            || plan.and_then(|p| p.manifest_digest.as_ref()) != Some(digest)
    }) || run.expected_partition.as_ref().is_some_and(|partition| {
        plan.and_then(|p| p.expected_partition.as_ref()) != Some(partition)
    }) {
        return Err(IngestError::Invalid("invalid shard plan"));
    }
    let tail = &trace.events()[1..];
    let logical = tail.is_empty()
        && definition.is_none()
        && inputs.is_empty()
        && (origin.is_none() || *origin == Some(RunOrigin::Invocation))
        && bundle.artifacts.is_empty()
        && run.shard.is_none()
        && run.manifest_digest.is_none()
        && run.expected_partition.is_none();
    if logical {
        if run.verdict.is_some() || run.finished_at.is_some() || run.duration_ms.is_some() {
            return Err(bad);
        }
    } else {
        let Some((last, middle)) = tail.split_last() else {
            return Err(bad);
        };
        if middle.iter().any(|event| {
            matches!(
                event.payload,
                EventPayload::RunStarted { .. }
                    | EventPayload::RunFinished { .. }
                    | EventPayload::RunAborted { .. }
            )
        }) {
            return Err(bad);
        }
        let verdict = match last.payload {
            EventPayload::RunFinished {
                verdict,
                body_verdict,
            } if verdict.is_some() || body_verdict.is_none() => verdict,
            EventPayload::RunAborted { .. } => None,
            _ => return Err(bad),
        };
        let duration = (last.ts - started_at).num_milliseconds().max(0) as u64;
        if run.verdict != verdict
            || run.finished_at.as_deref().and_then(|s| parse_ts(s).ok()) != Some(last.ts)
            || run.duration_ms != Some(duration)
        {
            return Err(bad);
        }
        if verdict.is_some() {
            replay(&trace).map_err(|_| IngestError::Invalid("judged events failed replay"))?;
        } else if matches!(last.payload, EventPayload::RunFinished { .. })
            && (definition.is_some()
                || middle.iter().any(|e| {
                    matches!(
                        e.payload,
                        EventPayload::AssertionEvaluated { .. }
                            | EventPayload::CheckFinished { .. }
                            | EventPayload::CriterionFinished { .. }
                    )
                }))
        {
            return Err(bad);
        }
    }
    let meta = RunMeta {
        run_id: run.run_id.clone(),
        verification: run.verification.clone(),
        schema_version: run.schema_version.clone(),
        inputs: run.inputs.clone(),
        started_at,
        lineage: RunLineage {
            parent_run_id: parent_run_id.clone(),
            origin: *origin,
        },
        scope: RunScope {
            project_id: run.project_id.clone(),
            verifier_repo: run.verifier_repo.clone(),
            verifier_sha: run.verifier_sha.clone(),
            target_repo: run.target_repo.clone(),
            target_sha: run.target_sha.clone(),
        },
    };
    Ok((meta, trace, logical))
}

fn validate_artifacts(bundle: &RunBundle) -> Result<Vec<(&str, Vec<u8>)>, IngestError> {
    let refs = referenced_blobs(&bundle.events);
    if !bundle.artifacts.iter().map(|a| &a.sha256).eq(&refs) {
        return Err(IngestError::Invalid(
            "artifacts must match the sorted event references",
        ));
    }
    bundle
        .artifacts
        .iter()
        .map(|artifact| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&artifact.bytes_base64)
                .map_err(|_| IngestError::Invalid("invalid artifact encoding"))?;
            if manifest_digest(&bytes) != artifact.sha256 {
                return Err(IngestError::Invalid("artifact digest does not match bytes"));
            }
            Ok((artifact.sha256.as_str(), bytes))
        })
        .collect()
}
