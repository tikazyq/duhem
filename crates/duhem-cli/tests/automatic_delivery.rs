//! Real CLI -> real dashboard ingest -> SQLite projections. No transport stubs.
#![cfg(unix)]
use std::{
    path::Path,
    process::{Command, Output},
    sync::Arc,
};

use duhem_evidence::{RunBundle, SqliteStore, Store};

fn command(path: &Path, db: &Path, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_duhem"));
    command
        .args([
            "run",
            "--no-live",
            "--no-env-file",
            "--reporter",
            "json",
            "--db",
        ])
        .arg(db)
        .arg(path)
        .env("DUHEM_HUB_URL", url)
        .env("DUHEM_HUB_TOKEN", "test-token");
    for key in [
        "DUHEM_PARENT_RUN_ID",
        "DUHEM_PARENT_DB_PATH",
        "DUHEM_SHARD",
        "DUHEM_RUN_ID",
        "CI_NODE_INDEX",
        "CI_NODE_TOTAL",
        "BUILDKITE_PARALLEL_JOB",
        "BUILDKITE_PARALLEL_JOB_COUNT",
        "CIRCLE_NODE_INDEX",
        "CIRCLE_NODE_TOTAL",
    ] {
        command.env_remove(key);
    }
    command
}

fn leaf(path: &Path, assertion: &str) {
    std::fs::write(path, format!("verification: delivery\ncriteria:\n  - id: A\n    description: Evidence arrives unchanged.\n    checks:\n      - id: a\n        assertions: ['{assertion}']\n  - id: B\n    description: Another shard unit.\n    checks:\n      - id: b\n        assertions: ['true']\n")).unwrap();
}

async fn invoke(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

async fn server(store: Arc<SqliteStore>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/ingest", listener.local_addr().unwrap());
    let reader = duhem_dashboard::EvidenceReader::new(Arc::new(
        SqliteStore::open_read_only(store.db_path()).await.unwrap(),
    ));
    let app = duhem_dashboard::router(reader).merge(duhem_dashboard::ingest::router(
        store,
        Some("test-token".into()),
    ));
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_and_failed_runs_arrive_and_resending_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = Arc::new(SqliteStore::open(tmp.path().join("hub.db")).await.unwrap());
    let (url, task) = server(hub.clone()).await;
    let path = tmp.path().join("leaf.yml");
    for (index, assertion) in ["true", "false"].iter().enumerate() {
        leaf(&path, assertion);
        let db = tmp.path().join(format!("source-{index}.db"));
        let output = invoke(command(&path, &db, &url)).await;
        assert_eq!(
            output.status.success(),
            index == 0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("automatic run delivery failed"));
        let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = summary["run_id"].as_str().unwrap();
        let record = hub.get_run(id).await.unwrap().unwrap();
        assert_eq!(
            record.verdict.unwrap().to_string(),
            if index == 0 { "pass" } else { "fail" }
        );
        let source = SqliteStore::open_read_only(db).await.unwrap();
        let bundle = RunBundle::from_store(&source, id).await.unwrap();
        assert_eq!(hub.run_events(id).await.unwrap(), bundle.events);
        assert!(
            !hub.ingest_bundle(&bundle, &bundle.content_hash().unwrap())
                .await
                .unwrap()
        );
    }
    assert_eq!(hub.list_runs().await.unwrap().len(), 2);
    let mut unconfigured = command(&path, &tmp.path().join("local-only.db"), &url);
    unconfigured.env_remove("DUHEM_HUB_URL");
    let output = invoke(unconfigured).await;
    assert!(
        !output.status.success(),
        "the fixture still has a false assertion"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("automatic run delivery"));
    assert_eq!(hub.list_runs().await.unwrap().len(), 2);
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn sharded_manifest_and_nested_invocation_arrive_parent_first() {
    let tmp = tempfile::tempdir().unwrap();
    let hub = Arc::new(SqliteStore::open(tmp.path().join("hub.db")).await.unwrap());
    let (url, task) = server(hub.clone()).await;
    let suite = tmp.path().join("suite");
    std::fs::create_dir(&suite).unwrap();
    leaf(&suite.join("leaf.yml"), "true");
    std::fs::write(
        suite.join("duhem.yml"),
        "manifest_version: 1\nverifications: [{path: leaf.yml}]\n",
    )
    .unwrap();
    for shard in ["1/2", "2/2"] {
        let mut cmd = command(
            &suite,
            &tmp.path().join(format!("{}.db", shard.replace('/', "-"))),
            &url,
        );
        cmd.args(["--shard", shard, "--run-id", "shared-invocation"]);
        let output = invoke(cmd).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("automatic run delivery failed"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let runs = hub.list_runs().await.unwrap();
    assert_eq!(
        runs.len(),
        5,
        "one logical parent, two suite shards and two leaves"
    );
    let logical = runs
        .iter()
        .find(|run| run.lineage.parent_run_id.is_none())
        .unwrap();
    assert!(logical.verdict.is_none());
    assert!(logical.finished_at.is_none());
    assert_eq!(
        runs.iter()
            .filter(|run| run.lineage.origin == Some(duhem_evidence::RunOrigin::Shard))
            .count(),
        2
    );
    for run in &runs {
        let bundle = RunBundle::from_store(hub.as_ref(), &run.run_id)
            .await
            .unwrap();
        if run.lineage.parent_run_id.is_some() {
            assert!(bundle.run.shard.is_some());
        }
    }
    // A real nested CLI process inherits the parent's SQLite store and defers
    // delivery. Successful ingestion proves the enclosing parent arrived first.
    let nested = tmp.path().join("nested.yml");
    leaf(&nested, "true");
    let outer = tmp.path().join("outer.yml");
    let argv = serde_json::to_string(&vec![
        env!("CARGO_BIN_EXE_duhem"),
        "run",
        nested.to_str().unwrap(),
        "--no-env-file",
        "--no-live",
    ])
    .unwrap();
    std::fs::write(&outer, format!("verification: outer\ncriteria:\n  - id: OUTER\n    description: Nested evidence is delivered with its parent.\n    checks:\n      - id: nested\n        steps:\n          - id: invoke\n            uses: cli/invoke\n            with:\n              command: {argv}\n        assertions: ['$steps.invoke.outputs.exit_code == 0']\n")).unwrap();
    let output = invoke(command(&outer, &tmp.path().join("nested.db"), &url)).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("automatic run delivery failed"));
    let runs = hub.list_runs().await.unwrap();
    assert_eq!(runs.len(), 7);
    assert_eq!(
        runs.iter()
            .filter(|run| run.lineage.origin == Some(duhem_evidence::RunOrigin::Invocation))
            .count(),
        1
    );
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn delivery_failure_warns_once_and_preserves_pass_and_fail_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/api/ingest", listener.local_addr().unwrap());
    drop(listener);
    let path = tmp.path().join("leaf.yml");
    for (index, assertion) in ["true", "false"].iter().enumerate() {
        leaf(&path, assertion);
        let output = invoke(command(
            &path,
            &tmp.path().join(format!("{index}.db")),
            &url,
        ))
        .await;
        assert_eq!(output.status.success(), index == 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("warning: automatic run delivery failed:")
                .count(),
            1
        );
        let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(summary["verdict"], if index == 0 { "pass" } else { "fail" });
    }
}
