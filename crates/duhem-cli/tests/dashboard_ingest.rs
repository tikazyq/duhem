//! Real CLI, dashboard process and SQLite. The dashboard CI lane builds
//! both binaries before running these tests.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use duhem_evidence::{EventPayload, RunBundle, RunOrigin, SqliteStore, Store, VerdictState};
use reqwest::{Client, StatusCode};

fn dashboard_bin() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_duhem")).with_file_name("duhem-dashboard")
}

struct Dashboard {
    child: Child,
    base_url: String,
}

impl Dashboard {
    fn start(db: &Path, flags: &[&str], token: Option<&str>) -> Self {
        let mut child = Command::new(dashboard_bin())
            .args(["--port", "0", "--db"])
            .arg(db)
            .args(flags)
            .env_remove("DUHEM_HUB_TOKEN")
            .envs(token.map(|token| ("DUHEM_HUB_TOKEN", token)))
            .stdout(Stdio::piped())
            .spawn()
            .expect("build the dashboard binary before running this test");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let base_url = line
            .split_whitespace()
            .find(|word| word.starts_with("http://"))
            .expect("dashboard listen announcement")
            .trim_end_matches('/')
            .to_owned();
        Self { child, base_url }
    }

    fn url(&self) -> String {
        format!("{}/api/ingest", self.base_url)
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(path: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_duhem"));
    cmd.args([
        "run",
        "--no-live",
        "--no-env-file",
        "--reporter",
        "json",
        "--db",
    ])
    .arg(db)
    .arg(path)
    .current_dir(path.parent().unwrap());
    for key in [
        "DUHEM_HUB_URL",
        "DUHEM_HUB_TOKEN",
        "DUHEM_SHARD",
        "CI_NODE_INDEX",
        "BUILDKITE_PARALLEL_JOB",
        "BUILDKITE_PARALLEL_JOB_COUNT",
        "CIRCLE_NODE_INDEX",
        "DUHEM_PARENT_RUN_ID",
        "DUHEM_PARENT_DB_PATH",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

fn pass(out: Output) {
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("warning: automatic evidence delivery"),
        "{out:?}"
    );
}

async fn post(client: &Client, url: &str, body: Vec<u8>, token: Option<&str>) -> StatusCode {
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-duhem-bundle-version", "1")
        .header(
            "x-duhem-content-hash",
            duhem_evidence::distributed::manifest_digest(&body),
        )
        .body(body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap().status()
}

const FIXTURE: &str = "verification: delivered\ncriteria:\n  - {id: A, description: arithmetic, checks: [{id: a, assertions: ['true']}]}\n";
const TOKEN: &str = "test-ingest-token";

#[tokio::test]
#[ignore = "needs the dashboard binary built (dashboard CI lane)"]
async fn ingest_auth_bounds_replay_idempotency_and_real_projections() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("hub.db");
    let source_db = tmp.path().join("source.db");
    let vd = tmp.path().join("verification.yml");
    std::fs::write(&vd, FIXTURE).unwrap();
    pass(
        run(&vd, &source_db)
            .args(["--run-id", "replica"])
            .output()
            .unwrap(),
    );
    let source = SqliteStore::open_read_only(&source_db).await.unwrap();
    let bundle = RunBundle::from_store(&source, "replica").await.unwrap();
    let body = bundle.wire_bytes().unwrap();
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let denied = Command::new(dashboard_bin())
        .args(["--ingest", "--host", "0.0.0.0", "--port", "0", "--db"])
        .arg(&db)
        .env("DUHEM_HUB_TOKEN", " ")
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("non-empty DUHEM_HUB_TOKEN"));
    assert!(!db.exists());
    let disabled = Dashboard::start(&source_db, &[], None);
    assert!(
        !post(&client, &disabled.url(), body.clone(), None)
            .await
            .is_success()
    );

    let hub = Dashboard::start(&db, &["--ingest"], Some(TOKEN));
    let url = hub.url();
    assert_eq!(
        post(&client, &url, body.clone(), None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&client, &url, body.clone(), Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&client, &url, vec![b' '; 16 * 1024 * 1024 + 1], Some(TOKEN)).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let mut invalid = bundle.clone();
    for event in &mut invalid.events {
        if let EventPayload::AssertionEvaluated { state, .. } = &mut event.payload {
            *state = VerdictState::Fail;
        }
    }
    assert_eq!(
        post(&client, &url, invalid.wire_bytes().unwrap(), Some(TOKEN)).await,
        StatusCode::BAD_REQUEST
    );
    let mut broken = bundle.clone();
    broken.artifacts.push(duhem_evidence::BundleArtifact {
        sha256: "f".repeat(64),
        bytes_base64: "eA==".into(),
        mask_counts: Default::default(),
    });
    assert_eq!(
        post(&client, &url, broken.wire_bytes().unwrap(), Some(TOKEN)).await,
        StatusCode::BAD_REQUEST
    );
    let store = SqliteStore::open_read_only(&db).await.unwrap();
    assert!(store.list_runs().await.unwrap().is_empty());
    let (a, b) = tokio::join!(
        post(&client, &url, body.clone(), Some(TOKEN)),
        post(&client, &url, body, Some(TOKEN))
    );
    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    assert_eq!(store.list_runs().await.unwrap().len(), 1);
    assert_eq!(store.run_events("replica").await.unwrap(), bundle.events);
    let history = store.criterion_history("verification").await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].verdict, VerdictState::Pass);
    let mut conflict = bundle;
    conflict.run.target_sha = Some("different".into());
    assert_eq!(
        post(&client, &url, conflict.wire_bytes().unwrap(), Some(TOKEN)).await,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
#[ignore = "needs the dashboard binary built (dashboard CI lane)"]
async fn automatic_delivery_preserves_nested_and_sharded_trees_and_native_blobs() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("hub.db");
    let hub = Dashboard::start(&db, &["--ingest"], None);
    let vd = tmp.path().join("inner.yml");
    let outer = tmp.path().join("outer.yml");
    let manifest = tmp.path().join("duhem.yml");
    std::fs::write(&vd, FIXTURE).unwrap();
    let argv = serde_json::json!([
        env!("CARGO_BIN_EXE_duhem"),
        "run",
        &vd,
        "--no-live",
        "--run-id",
        "nested"
    ]);
    std::fs::write(&outer, format!(r#"verification: outer
native:
  - id: junit
    command: [sh, -c, 'printf "%s" "<testsuite name=\"unit\" tests=\"1\"><testcase name=\"works\"/></testsuite>" > junit.xml']
    report: {{format: junit, path: junit.xml}}
criteria:
  - id: OUTER
    description: Nested CLI completes
    checks:
      - id: nested-cli
        steps:
          - id: nested
            uses: cli/invoke
            with:
              command: {argv}
              env: {{DUHEM_HUB_URL: "{}"}}
        assertions:
          - $steps.nested.outputs.exit_code == 0
          - $steps.nested.outputs.stderr == ''
"#, hub.url())).unwrap();
    std::fs::write(
        &manifest,
        "manifest_version: 1\nverifications: [{path: outer.yml}, {path: inner.yml}]\n",
    )
    .unwrap();
    for index in [1, 2] {
        pass(
            run(&manifest, &tmp.path().join(format!("shard-{index}.db")))
                .args(["--shard", &format!("{index}/2"), "--run-id", "matrix"])
                .env("DUHEM_HUB_URL", hub.url())
                .output()
                .unwrap(),
        );
    }
    let store = SqliteStore::open_read_only(&db).await.unwrap();
    let records = store.list_runs().await.unwrap();
    let parent = records
        .iter()
        .find(|r| r.lineage.parent_run_id.is_none())
        .unwrap();
    assert_eq!(
        parent.run_id,
        duhem_evidence::distributed::logical_run_id("matrix")
    );
    assert_eq!(parent.verdict, None);
    assert_eq!(parent.finished_at, None);
    let detail: serde_json::Value = Client::new()
        .get(format!("{}/api/runs/{}", hub.base_url, parent.run_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(detail["verdict"].is_null());
    let shards: Vec<_> = records
        .iter()
        .filter(|r| r.lineage.origin == Some(RunOrigin::Shard))
        .collect();
    assert_eq!(shards.len(), 2);
    for shard in shards {
        assert_eq!(
            shard.lineage.parent_run_id.as_deref(),
            Some(parent.run_id.as_str())
        );
        let bundle = RunBundle::from_store(&store, &shard.run_id).await.unwrap();
        assert_eq!(bundle.run.shard.unwrap().total, 2);
        assert!(bundle.run.manifest_digest.is_some());
        assert!(bundle.run.expected_partition.is_some());
    }
    let nested = store.get_run("nested").await.unwrap().unwrap();
    assert_eq!(nested.lineage.origin, Some(RunOrigin::Invocation));
    let outer_id = nested.lineage.parent_run_id.unwrap();
    let mut native_refs = 0;
    // The native unit and criterion may land on different shards.
    for record in &records {
        for event in store.run_events(&record.run_id).await.unwrap() {
            if let EventPayload::NativeRunFinished { result } = event.payload {
                for sha in [
                    result.report_blob_sha256.unwrap(),
                    result.results_blob_sha256.unwrap(),
                ] {
                    native_refs += 1;
                    assert!(store.get_blob(&sha).await.unwrap().is_some());
                }
            }
        }
    }
    assert_eq!(native_refs, 2);
    assert_eq!(
        store
            .get_run(&outer_id)
            .await
            .unwrap()
            .unwrap()
            .lineage
            .origin,
        Some(RunOrigin::Suite)
    );

    for (assertion, expected) in [("true", 0), ("false", 1)] {
        let leaf = tmp.path().join(format!("{assertion}.yml"));
        std::fs::write(&leaf, FIXTURE.replace("true", assertion)).unwrap();
        let out = run(&leaf, &tmp.path().join(format!("{assertion}.db")))
            .env("DUHEM_HUB_URL", hub.url())
            .env("DUHEM_HUB_TOKEN", "bad\nheader")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(expected), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr)
                .matches("warning: automatic evidence delivery")
                .count(),
            1,
            "{out:?}"
        );
    }
}
