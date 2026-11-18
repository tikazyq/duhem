//! HTTP validation, SQLite atomicity and independent replay of real evidence.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use duhem_evidence::{
    EventPayload, EvidenceWriter, RunBundle, SqliteStore, Store, VerdictState, run_started,
};
use std::{collections::BTreeMap, sync::Arc};
use tower::ServiceExt;

async fn bundle(store: Arc<SqliteStore>, id: &str) -> RunBundle {
    let mut writer = EvidenceWriter::begin(store.clone(), id, "fixture.yml", BTreeMap::new())
        .await
        .unwrap();
    writer
        .append(run_started("fixture.yml", BTreeMap::new()))
        .await
        .unwrap();
    for payload in [
        serde_json::json!({"kind":"assertion_evaluated", "check_id":"check", "assertion_index":0, "state":"pass"}),
        serde_json::json!({"kind":"check_finished", "check_id":"check", "criterion_id":"criterion", "verdict":"pass"}),
        serde_json::json!({"kind":"criterion_finished", "criterion_id":"criterion", "verdict":"pass"}),
    ] {
        writer
            .append(serde_json::from_value(payload).unwrap())
            .await
            .unwrap();
    }
    let report = writer.write_blob(b"report").await.unwrap().0;
    let results = writer.write_blob(b"results").await.unwrap().0;
    writer
        .append(EventPayload::NativeRunFinished {
            result: serde_json::from_value(serde_json::json!({
                "id": "native", "outcome": "pass", "duration_ms": 1, "required": true, "tests": 1,
                "report_blob_sha256": report,
                "results_blob_sha256": results
            }))
            .unwrap(),
        })
        .await
        .unwrap();
    writer
        .append(EventPayload::RunFinished {
            body_verdict: None,
            verdict: Some(VerdictState::Pass),
        })
        .await
        .unwrap();
    RunBundle::from_store(store.as_ref(), id).await.unwrap()
}

async fn post(app: axum::Router, bundle: &RunBundle, token: Option<&str>) -> StatusCode {
    let mut request = Request::post("/api/ingest")
        .header("content-type", "application/json")
        .header("x-duhem-bundle-version", "1")
        .header("x-duhem-content-hash", bundle.content_hash().unwrap());
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(
        request
            .body(Body::from(bundle.wire_bytes().unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn auth_bounds_headers_replay_artifacts_and_idempotency() {
    let tmp = tempfile::tempdir().unwrap();
    let source = Arc::new(
        SqliteStore::open(tmp.path().join("source.db"))
            .await
            .unwrap(),
    );
    let target = Arc::new(
        SqliteStore::open(tmp.path().join("target.db"))
            .await
            .unwrap(),
    );
    let bundle = bundle(source, "ingest-test").await;
    assert_eq!(
        bundle.artifacts.len(),
        2,
        "native report and result blobs travel"
    );
    let app = duhem_dashboard::ingest::router(target.clone(), Some("secret".into()));
    assert_eq!(
        post(app.clone(), &bundle, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(app.clone(), &bundle, Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    let bad = Request::post("/api/ingest")
        .header("authorization", "Bearer secret")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(bad).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let large = Request::post("/api/ingest")
        .body(Body::from(vec![
            0;
            duhem_dashboard::ingest::MAX_BUNDLE_BYTES
                + 1
        ]))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(large).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert!(target.list_runs().await.unwrap().is_empty());
    let wrong_hash = Request::post("/api/ingest")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .header("x-duhem-bundle-version", "1")
        .header("x-duhem-content-hash", "00".repeat(32))
        .body(Body::from(bundle.wire_bytes().unwrap()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(wrong_hash).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut gap = bundle.clone();
    gap.events[1].seq += 1;
    assert_eq!(
        post(app.clone(), &gap, Some("secret")).await,
        StatusCode::BAD_REQUEST
    );
    let mut missing = bundle.clone();
    missing.artifacts.clear();
    assert_eq!(
        post(app.clone(), &missing, Some("secret")).await,
        StatusCode::BAD_REQUEST
    );
    let mut corrupt = bundle.clone();
    corrupt.artifacts[0].bytes_base64 = "AAAA".into();
    assert_eq!(
        post(app.clone(), &corrupt, Some("secret")).await,
        StatusCode::BAD_REQUEST
    );
    let mut divergent = bundle.clone();
    divergent.run.verdict = Some(VerdictState::Fail);
    if let EventPayload::RunFinished { verdict, .. } =
        &mut divergent.events.last_mut().unwrap().payload
    {
        *verdict = Some(VerdictState::Fail);
    }
    assert_eq!(
        post(app.clone(), &divergent, Some("secret")).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(app.clone(), &bundle, Some("secret")).await,
        StatusCode::OK
    );
    assert_eq!(
        post(app.clone(), &bundle, Some("secret")).await,
        StatusCode::OK
    );
    assert_eq!(target.list_runs().await.unwrap().len(), 1);
    for table in [
        "assertions",
        "checks",
        "criteria",
        "run_verdicts",
        "run_terminals",
        "ingest_receipts",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(target.pool())
            .await
            .unwrap();
        assert_eq!(count, 1, "normal projection {table}");
    }
    assert_eq!(
        target.run_events(&bundle.run.run_id).await.unwrap(),
        bundle.events
    );
    for artifact in &bundle.artifacts {
        assert!(target.get_blob(&artifact.sha256).await.unwrap().is_some());
    }
    let mut conflict = bundle.clone();
    conflict.run.project_id = Some("different-project".into());
    assert_eq!(
        post(app, &conflict, Some("secret")).await,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn failed_projection_rolls_back_run_blobs_events_and_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let source = Arc::new(
        SqliteStore::open(tmp.path().join("source.db"))
            .await
            .unwrap(),
    );
    let target = Arc::new(
        SqliteStore::open(tmp.path().join("target.db"))
            .await
            .unwrap(),
    );
    let bundle = bundle(source, "atomic-test").await;
    // Inject a real SQLite projection failure after the header, artifacts and
    // events have started inserting. All of them must roll back with the receipt.
    sqlx::query("CREATE TRIGGER reject_terminal BEFORE INSERT ON run_terminals BEGIN SELECT RAISE(ABORT, 'test projection failure'); END")
        .execute(target.pool()).await.unwrap();
    let app = duhem_dashboard::ingest::router(target.clone(), None);
    assert_eq!(
        post(app.clone(), &bundle, None).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(target.list_runs().await.unwrap().is_empty());
    for table in [
        "events",
        "artifacts",
        "ingest_receipts",
        "run_verdicts",
        "verifications",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(target.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "{table} was partially committed");
    }
    sqlx::query("DROP TRIGGER reject_terminal")
        .execute(target.pool())
        .await
        .unwrap();
    assert_eq!(post(app, &bundle, None).await, StatusCode::OK);
}

#[test]
fn public_bind_without_token_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_duhem-dashboard"))
        .args(["--ingest", "--host", "0.0.0.0", "--port", "0", "--db"])
        .arg(tmp.path().join("hub.db"))
        .env_remove("DUHEM_HUB_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-empty DUHEM_HUB_TOKEN"));
}
