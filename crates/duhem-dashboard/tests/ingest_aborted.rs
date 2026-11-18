mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use duhem_dashboard::{EvidenceReader, ingest::ingest_router};
use duhem_evidence::{
    EventPayload, EvidenceWriter, RunBundle, SecretRegistry, Store, VerdictState, run_started,
};
use std::collections::BTreeMap;
use tower::ServiceExt;

async fn upload(
    store: std::sync::Arc<duhem_evidence::SqliteStore>,
    bundle: &RunBundle,
) -> StatusCode {
    ingest_router(store, None)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/ingest")
                .header("content-type", "application/json")
                .header("x-duhem-bundle-version", "1")
                .header("x-duhem-content-hash", bundle.content_hash().unwrap())
                .body(Body::from(bundle.wire_bytes().unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ingest_replays_aborted_judgments_and_dashboard_shows_masked_error() {
    let (_producer_tmp, producer, _) = common::open_stores().await;
    let (_receiver_tmp, receiver, reader_store) = common::open_stores().await;
    let mut secrets = SecretRegistry::new();
    let secret = "private-engine-error-token";
    secrets.register_json("token", &serde_json::json!(secret));
    let mut writer = EvidenceWriter::begin_scoped_with_secrets(
        producer.clone(),
        "aborted",
        "abort.yml",
        BTreeMap::new(),
        Default::default(),
        secrets,
    )
    .await
    .unwrap();
    for payload in [
        run_started("abort.yml", BTreeMap::new()),
        EventPayload::AssertionEvaluated {
            check_id: "done".into(),
            diagnostic: false,
            operands: None,
            assertion_index: 0,
            iteration: None,
            state: VerdictState::Pass,
            detail: None,
            expr: Some("true".into()),
            step_index: None,
        },
        EventPayload::CheckFinished {
            check_id: "done".into(),
            criterion_id: Some("complete".into()),
            gated_judging_steps: 0,
            verdict: VerdictState::Pass,
            session_source: None,
            session_digest: None,
        },
        EventPayload::CriterionFinished {
            criterion_id: "complete".into(),
            verdict: VerdictState::Pass,
        },
        EventPayload::AssertionEvaluated {
            check_id: "checked".into(),
            diagnostic: false,
            operands: None,
            assertion_index: 0,
            iteration: None,
            state: VerdictState::Pass,
            detail: None,
            expr: Some("true".into()),
            step_index: None,
        },
        EventPayload::CheckFinished {
            check_id: "checked".into(),
            criterion_id: Some("open".into()),
            gated_judging_steps: 0,
            verdict: VerdictState::Pass,
            session_source: None,
            session_digest: None,
        },
        EventPayload::StepStarted {
            criterion_id: "open".into(),
            check_id: "unfinished".into(),
            step_index: 0,
            uses: "cli/invoke".into(),
            layer: None,
            with: BTreeMap::new(),
            flow: None,
            group: None,
        },
        EventPayload::RunAborted {
            signal: "engine_error".into(),
            error: Some(format!("unresolved reference: {secret}")),
        },
    ] {
        writer.append(payload).await.unwrap();
    }
    writer.finish().await.unwrap();
    let bundle = RunBundle::from_store(producer.as_ref(), "aborted")
        .await
        .unwrap();

    // Rehash each tampered bundle so validation has to recompute judgments.
    for index in [1, 2, 3, 4, 5] {
        let mut tampered = bundle.clone();
        match &mut tampered.events[index].payload {
            EventPayload::AssertionEvaluated { state, .. } => *state = VerdictState::Fail,
            EventPayload::CheckFinished { verdict, .. }
            | EventPayload::CriterionFinished { verdict, .. } => *verdict = VerdictState::Fail,
            _ => unreachable!(),
        }
        assert_eq!(
            upload(receiver.clone(), &tampered).await,
            StatusCode::BAD_REQUEST
        );
        assert!(receiver.get_run("aborted").await.unwrap().is_none());
    }
    assert_eq!(
        upload(receiver.clone(), &bundle).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        upload(receiver.clone(), &bundle).await,
        StatusCode::NO_CONTENT
    );
    let record = receiver.get_run("aborted").await.unwrap().unwrap();
    assert_eq!(record.status, duhem_evidence::RunStatus::Aborted);
    assert_eq!(record.verdict, None);
    let reader = EvidenceReader::new(reader_store);
    let detail = reader.run_detail("aborted").await.unwrap().unwrap();
    assert_eq!(detail.status, duhem_evidence::RunStatus::Aborted);
    assert_eq!(detail.verdict, None);
    assert_eq!(
        detail.error.as_deref(),
        Some("unresolved reference: [redacted:token]")
    );
    assert_eq!(detail.criteria[0].verdict, Some(VerdictState::Pass));
    assert_eq!(detail.criteria[1].verdict, None);
}
