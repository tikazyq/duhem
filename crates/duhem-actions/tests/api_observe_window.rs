//! Real Chromium request-window regressions (#663). Run in browser CI.
use std::collections::BTreeMap;
use std::time::Duration;

use axum::{
    Router,
    response::{Html, Redirect},
    routing::get,
};
use duhem_actions::{Action, ActionCtx, Observe, RunBrowser, action::StepTime};
use serde_json::{Value, json};

async fn observe(ctx: &ActionCtx<'_>, duration: u64) -> Value {
    let with = serde_yml::from_str(&format!("url_pattern: 're:/request/'\nwindow: {{ from_step: anchor, edge: finish, duration: {duration} }}")).unwrap();
    let result = Observe.invoke(ctx, &with).await.unwrap();
    serde_json::to_value(result.outputs).unwrap()
}

#[tokio::test]
#[ignore = "requires `npx playwright install chromium`"]
async fn ordered_duplicates_redirects_aborts_pending_and_outside_window() {
    let app = Router::new()
        .route("/", get(|| async { Html("<html>fixture</html>") }))
        .route(
            "/request/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                "slow"
            }),
        )
        .route("/request/fast", get(|| async { "fast" }))
        .route(
            "/request/redirect",
            get(|| async { Redirect::temporary("/request/fast") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let browser = RunBrowser::launch(false).await.unwrap();
    let check = browser.open_check().await.unwrap();
    let page = &check.page;
    // Navigate through the public action so the page has finished loading.
    duhem_actions::Navigate
        .invoke(
            &ActionCtx {
                page: Some(page),
                ..Default::default()
            },
            &serde_yml::from_str(&format!("url: http://{addr}/")).unwrap(),
        )
        .await
        .unwrap();
    page.eval::<Value>("fetch('/request/fast').then(() => null)")
        .await
        .unwrap();
    let anchor = page.session_time_ms().await.unwrap();
    let times = BTreeMap::from([(
        "anchor".into(),
        StepTime {
            session: None,
            start_ms: anchor,
            finish_ms: anchor,
        },
    )]);
    let ctx = ActionCtx {
        page: Some(page),
        step_index: 1,
        step_times: Some(&times),
        ..Default::default()
    };
    page.eval::<Value>(
        r#"(() => {
      fetch('/request/slow');
      fetch('/request/fast'); fetch('/request/fast');
      fetch('/request/redirect');
      const controller = new AbortController();
      fetch('/request/slow?abort', { signal: controller.signal }).catch(() => {});
      setTimeout(() => controller.abort(), 100);
      return null;
    })()"#,
    )
    .await
    .unwrap();
    let result = observe(&ctx, 1000).await;
    assert_eq!(result["count"], 6);
    assert_eq!(result["complete"], true);
    assert_eq!(result["truncated"], false);
    let requests = result["requests"].as_array().unwrap();
    assert!(
        requests
            .windows(2)
            .all(|r| r[0]["sequence"].as_u64() < r[1]["sequence"].as_u64())
    );
    assert!(
        requests[0]["url"]
            .as_str()
            .unwrap()
            .ends_with("/request/slow")
    );
    assert_eq!(requests[0]["state"], "pending");
    assert_eq!(requests[1]["url"], requests[2]["url"]);
    assert!(requests.iter().any(|r| r["status"] == 307));
    assert!(requests.iter().any(|r| r["state"] == "failed"));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["url"].as_str().unwrap().ends_with("/request/fast"))
            .count(),
        3
    );
    // A later request and later terminal states must not change the snapshot.
    page.eval::<Value>("fetch('/request/fast').then(() => null)")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(observe(&ctx, 1000).await, result);
    check.close(false, 0).await.unwrap();
    server.abort();
}

#[tokio::test]
#[ignore = "requires `npx playwright install chromium`"]
async fn overflow_and_missing_or_cross_session_anchor() {
    let browser = RunBrowser::launch(false).await.unwrap();
    let check = browser.open_check().await.unwrap();
    let page = &check.page;
    let anchor = page.session_time_ms().await.unwrap();
    let times = BTreeMap::from([(
        "anchor".into(),
        StepTime {
            session: Some("other".into()),
            start_ms: anchor,
            finish_ms: anchor,
        },
    )]);
    let with = serde_yml::from_str(
        "url_pattern: 're:.*'\nwindow: { from_step: anchor, edge: start, duration: 1 }",
    )
    .unwrap();
    let mut ctx = ActionCtx {
        page: Some(page),
        step_times: Some(&times),
        ..Default::default()
    };
    assert!(
        Observe
            .invoke(&ctx, &with)
            .await
            .unwrap_err()
            .to_string()
            .contains("different browser session")
    );
    ctx.step_times = None;
    assert!(
        Observe
            .invoke(&ctx, &with)
            .await
            .unwrap_err()
            .to_string()
            .contains("missing")
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/request/fast", get(|| async { "ok" }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    duhem_actions::Navigate
        .invoke(
            &ActionCtx {
                page: Some(page),
                ..Default::default()
            },
            &serde_yml::from_str(&format!("url: http://{addr}/request/fast")).unwrap(),
        )
        .await
        .unwrap();
    let anchor = page.session_time_ms().await.unwrap();
    let times = BTreeMap::from([(
        "anchor".into(),
        StepTime {
            session: None,
            start_ms: anchor,
            finish_ms: anchor,
        },
    )]);
    let ctx = ActionCtx {
        page: Some(page),
        step_times: Some(&times),
        ..Default::default()
    };
    page.eval::<Value>("(async () => { for (let i = 0; i < 1001; i++) await fetch('/request/fast?i=' + i); return null })()").await.unwrap();
    let elapsed = page.session_time_ms().await.unwrap() - anchor;
    let result = observe(&ctx, elapsed.ceil() as u64 + 100).await;
    assert_eq!(result["count"], 1000);
    assert_eq!(result["complete"], false);
    assert_eq!(result["truncated"], true);
    assert_eq!(
        result["requests"][0]["url"],
        json!(format!("http://{addr}/request/fast?i=0"))
    );
    check.close(false, 0).await.unwrap();
    server.abort();
}
