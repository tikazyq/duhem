//! End-to-end smoke for `api/observe` against an in-process axum
//! server. Spec on issue #38.
//!
//! The fixture serves a tiny HTML page with a button that schedules a
//! `fetch('/projects', { method: 'POST', body: '{"name":"Acme"}' })`
//! 200 ms after click. The Verification Definition lives at
//! `crates/duhem-actions/tests/fixtures/api-observe.yml`; the engine
//! drives it: `ui/navigate` → `ui/click` → `api/observe`. With the v1
//! blocking listener, the delayed fetch fires while `api/observe` is
//! awaiting events, so the capture succeeds.
//!
//! `#[ignore]`'d for the same reason as `api_smoke.rs`: needs
//! `npx playwright install chromium`.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::Router;
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::{get, post};
use duhem_actions::RunBrowser;
use duhem_judge::VerdictState;
use duhem_runtime::Engine;
use duhem_schema::VerificationDefinition;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const FIXTURE_YAML: &str = include_str!("../../duhem-actions/tests/fixtures/api-observe.yml");

const HTML: &str = r#"<!doctype html>
<html><body>
<button id="t">trigger</button>
<button id="collect" onclick="(async () => {
  await fetch('/traffic/first?token=' + new URLSearchParams(location.search).get('token'));
  await fetch('/traffic/second');
})()">collect</button>
<script>
document.getElementById('t').addEventListener('click', () => {
  // 200ms delay so api/observe has time to subscribe to the page's
  // event stream before this fetch fires. v1 listener attaches at
  // observe-step runtime, not at check start.
  setTimeout(() => {
    fetch('/projects', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: 'Acme' }),
    });
  }, 200);
});
</script>
</body></html>
"#;

struct Fixture {
    addr: SocketAddr,
    _server: JoinHandle<()>,
}

async fn start_fixture() -> Fixture {
    let app = Router::new()
        .route("/", get(|| async { Html(HTML) }))
        .route("/traffic/{name}", get(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/projects",
            post(|| async {
                (
                    StatusCode::CREATED,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"id":"01h000000000000000000000"}"#,
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fixture {
        addr,
        _server: server,
    }
}

#[tokio::test]
#[ignore = "requires `npx playwright install chromium`"]
async fn api_observe_captures_clicked_post_and_passes_end_to_end() {
    let fx = start_fixture().await;
    let base_url = format!("http://{}/", fx.addr);

    let def = VerificationDefinition::from_yaml_str(FIXTURE_YAML).expect("parse fixture");

    let browser = RunBrowser::launch(false)
        .await
        .expect("launch chromium (run `npx playwright install chromium`)");

    let tmp = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        duhem_evidence::SqliteStore::open(tmp.path().join("duhem.db"))
            .await
            .expect("open store"),
    );
    let mut engine = Engine::new()
        .with_browser(browser)
        .with_store(store.clone());

    let mut inputs = BTreeMap::new();
    inputs.insert("base_url".to_string(), serde_json::Value::String(base_url));

    let verdict = engine.run(&def, inputs).await.expect("engine.run");
    assert_eq!(verdict.state, VerdictState::Pass, "verdict = {verdict:?}");
}

#[tokio::test]
#[ignore = "requires `npx playwright install chromium`"]
async fn observe_window_masks_urls_in_sqlite_and_rejects_unavailable_anchors() {
    use duhem_evidence::{EventPayload, ObservationValue, Trace};

    let fx = start_fixture().await;
    let def = VerificationDefinition::from_yaml_str(
        r#"
verification: request window evidence
inputs:
  base_url: {type: string}
  token: {type: string, secret: true}
criteria:
  - id: AC-1
    description: Preserve request order and mask the token in stored evidence.
    checks:
      - id: AC-1.1
        steps:
          - uses: ui/navigate
            with: {url: '$runtime.concat($inputs.base_url, "?token=", $inputs.token)'}
          - id: trigger
            uses: ui/click
            with: {role: button, name: collect}
          - id: observed
            uses: api/observe
            with:
              method: GET
              url_pattern: 're:/traffic/'
              window: {from_step: trigger, edge: start, duration: 2s}
        assertions:
          - $steps.observed.outputs.complete == true
          - $steps.observed.outputs.count == 2
      - id: AC-1.2
        sessions: {first: ~, second: ~}
        steps:
          - id: other
            uses: ui/navigate
            session: first
            with: {url: 'about:blank'}
          - uses: api/observe
            session: second
            with:
              method: GET
              url_pattern: 're:/traffic/'
              window: {from_step: other, edge: finish, duration: 1ms}
        assertions: ['true']
      - id: AC-1.3
        steps:
          - id: skipped
            uses: ui/navigate
            if: 'false'
            with: {url: 'about:blank'}
          - uses: api/observe
            with:
              method: GET
              url_pattern: 're:/traffic/'
              window: {from_step: skipped, edge: start, duration: 1ms}
        assertions: ['true']
"#,
    )
    .unwrap();
    let browser = RunBrowser::launch(false).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        duhem_evidence::SqliteStore::open(tmp.path().join("duhem.db"))
            .await
            .unwrap(),
    );
    let mut engine = Engine::new()
        .with_browser(browser)
        .with_store(store.clone())
        .with_capture(duhem_runtime::CapturePolicy::Off);
    let token = "window-test-secret-068f";
    let result = engine
        .run_with_metadata(
            &def,
            BTreeMap::from([
                (
                    "base_url".into(),
                    serde_json::json!(format!("http://{}/", fx.addr)),
                ),
                ("token".into(), serde_json::json!(token)),
            ]),
        )
        .await
        .unwrap();
    let trace = Trace::from_store(store.as_ref(), &result.run_id)
        .await
        .unwrap();
    let events = trace.events();
    let requests = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::StepObservation {
                output_name,
                value: ObservationValue::Inline { value },
                ..
            } if output_name == "requests" => value.as_array(),
            _ => None,
        })
        .expect("request observations");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["url"],
        format!("http://{}/traffic/first?token=[redacted:token]", fx.addr)
    );
    assert_eq!(
        requests[1]["url"],
        format!("http://{}/traffic/second", fx.addr)
    );
    let wire = serde_json::to_string(events).unwrap();
    assert!(!wire.contains(token));
    assert!(wire.contains("different browser session"));
    assert!(wire.contains("no browser timing"));
}
