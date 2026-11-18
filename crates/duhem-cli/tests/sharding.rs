#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use duhem_evidence::{EventPayload, RunOrigin, ShardPlan, SqliteStore, Store};
use serde_json::Value;

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../verifications/sharding-example/duhem.yml")
}

fn run(target: &Path, db: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_duhem"));
    command
        .args([
            "run",
            "--no-env-file",
            "--no-live",
            "--reporter",
            "json",
            "-f",
        ])
        .arg(target)
        .arg("--db")
        .arg(db);
    for key in [
        "DUHEM_SHARD",
        "DUHEM_RUN_ID",
        "DUHEM_PARENT_RUN_ID",
        "DUHEM_PARENT_DB_PATH",
        "CI_NODE_INDEX",
        "CI_NODE_TOTAL",
        "CI_PIPELINE_ID",
        "CI_JOB_NAME",
        "BUILDKITE_PARALLEL_JOB",
        "BUILDKITE_PARALLEL_JOB_COUNT",
        "BUILDKITE_BUILD_ID",
        "BUILDKITE_STEP_ID",
        "CIRCLE_NODE_INDEX",
        "CIRCLE_NODE_TOTAL",
        "CIRCLE_WORKFLOW_ID",
        "CIRCLE_JOB",
    ] {
        command.env_remove(key);
    }
    command
}

fn stdout(output: Output) -> String {
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    text
}

async fn evidence(db: &Path, summary: &Value) -> (String, ShardPlan) {
    let id = summary["run_id"].as_str().unwrap();
    let store = SqliteStore::open_read_only(db).await.unwrap();
    let record = store.get_run(id).await.unwrap().unwrap();
    assert_eq!(record.lineage.origin, Some(RunOrigin::Shard));
    let parent = record.lineage.parent_run_id.unwrap();
    let plan = record.lineage.shard.unwrap();
    assert_ne!(id, parent);
    let events = store.run_events(id).await.unwrap();
    assert!(matches!(
        &events[0].payload,
        EventPayload::RunStarted {
            origin: Some(RunOrigin::Shard), parent_run_id: Some(event_parent), shard: Some(event_plan), ..
        } if event_parent == &parent && event_plan == &plan
    ));
    (parent, plan)
}

#[tokio::test]
async fn manifest_shards_cover_the_plan_and_keep_checks_together() {
    let mut ids = BTreeSet::new();
    let mut parents = BTreeSet::new();
    let mut assignments = None;
    for index in [1, 2, 1] {
        // Use separate checkouts and stores, like runners on separate machines.
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("duhem.yml");
        let db = temp.path().join("evidence.db");
        std::fs::copy(example(), &manifest).unwrap();
        std::fs::copy(
            example().with_file_name("checks.yml"),
            temp.path().join("checks.yml"),
        )
        .unwrap();
        let text = stdout(
            run(&manifest, &db)
                .args(["--shard", &format!("{index}/2")])
                .output()
                .unwrap(),
        );
        let summary: Value = serde_json::from_str(&text).unwrap();
        assert!(ids.insert(summary["run_id"].as_str().unwrap().to_owned()));
        let (parent, plan) = evidence(&db, &summary).await;
        parents.insert(parent);
        assert_eq!(
            (plan.index, plan.total, plan.source.as_str()),
            (index, 2, "--shard")
        );
        assert_eq!(plan.manifest_digest.len(), 64);
        assert_eq!(plan.assignments.len(), 3);
        if let Some(expected) = &assignments {
            assert_eq!(&plan.assignments, expected);
        } else {
            assignments = Some(plan.assignments);
        }
        let criteria = summary["runs"][0]["criteria"].as_array().unwrap();
        assert_eq!(criteria.len(), 1);
        if index == 1 {
            assert_eq!(summary["native"][0]["id"], "git-version");
            assert_eq!(criteria[0]["id"], "OPTIONS");
        } else {
            assert!(summary["native"].as_array().is_none_or(Vec::is_empty));
            assert_eq!(criteria[0]["id"], "VERSION");
            assert_eq!(summary["totals"]["total"], 2);
        }
    }
    assert_eq!(parents.len(), 1);
}

#[test]
fn single_leaf_and_filtered_manifest_dry_runs() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("evidence.db");
    let leaf = example().with_file_name("checks.yml");
    let dry = |file: &Path, args: &[&str]| {
        stdout(run(file, &db).arg("--dry-run").args(args).output().unwrap())
    };
    assert_eq!(
        dry(&leaf, &[]),
        "WOULD RUN: VERSION::VERSION.1\nWOULD RUN: VERSION::VERSION.2\nWOULD RUN: OPTIONS::OPTIONS.1\nRESOLVED INPUT: (none)\n"
    );
    let first = dry(&leaf, &["--shard", "1/2"]);
    assert!(first.contains("VERSION::VERSION.1\nWOULD RUN: VERSION::VERSION.2"));
    assert!(!first.contains("OPTIONS"));
    let second = dry(&leaf, &["--shard", "2/2"]);
    assert!(second.contains("WOULD RUN: OPTIONS::OPTIONS.1"));
    assert!(!second.contains("VERSION"));
    let filtered = dry(
        &example(),
        &[
            "--filter",
            "VERSION::VERSION.2",
            "--filter",
            "OPTIONS",
            "--shard",
            "1/2",
        ],
    );
    assert!(filtered.contains("WOULD RUN: checks::VERSION::VERSION.2"));
    assert!(
        !filtered.contains("VERSION.1")
            && !filtered.contains("OPTIONS")
            && !filtered.contains("NATIVE")
    );
    let native = dry(
        &example(),
        &["--filter", "native:git-version", "--shard", "1/2"],
    );
    assert!(native.contains("WOULD RUN NATIVE: git-version"));
    assert!(!db.exists());
}

#[test]
fn flags_then_generic_env_then_provider_env() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("evidence.db");
    let target = example();
    let dry = || {
        let mut command = run(&target, &db);
        command.arg("--dry-run");
        command
    };
    for (index, total, value) in [
        ("CI_NODE_INDEX", "CI_NODE_TOTAL", "2"),
        (
            "BUILDKITE_PARALLEL_JOB",
            "BUILDKITE_PARALLEL_JOB_COUNT",
            "1",
        ),
        ("CIRCLE_NODE_INDEX", "CIRCLE_NODE_TOTAL", "1"),
    ] {
        assert!(
            stdout(dry().env(index, value).env(total, "3").output().unwrap())
                .starts_with("SHARD: 2/3\n")
        );
    }
    assert!(
        stdout(
            dry()
                .env("CI_NODE_INDEX", "junk")
                .env("DUHEM_SHARD", "1/3")
                .output()
                .unwrap()
        )
        .starts_with("SHARD: 1/3\n")
    );
    assert!(
        stdout(
            dry()
                .env("DUHEM_SHARD", "junk")
                .args(["--shard", "2/2"])
                .output()
                .unwrap()
        )
        .starts_with("SHARD: 2/2\n")
    );
    assert!(!stdout(dry().env("CI_NODE_TOTAL", "1").output().unwrap()).contains("SHARD:"));
}

#[test]
fn invalid_shards_never_open_the_store_or_start_work() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("evidence.db");
    let target = temp.path().join("duhem.yml");
    std::fs::write(&target, "manifest_version: 1\nverifications: []\nnative:\n  - id: marker\n    command: [touch, ran]\n").unwrap();
    let assert_invalid = |output: Output| {
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("shard"));
        assert!(!db.exists());
        assert!(!temp.path().join("ran").exists());
    };
    for spec in ["0/2", "3/2", "1/0", "x/2", "1/two", "1", "1/2/3"] {
        assert_invalid(run(&target, &db).args(["--shard", spec]).output().unwrap());
    }
    assert_invalid(
        run(&target, &db)
            .env("DUHEM_SHARD", "0/1")
            .output()
            .unwrap(),
    );
    assert_invalid(
        run(&target, &db)
            .env("CIRCLE_NODE_INDEX", "2")
            .env("CIRCLE_NODE_TOTAL", "2")
            .output()
            .unwrap(),
    );
}

#[tokio::test]
async fn empty_manifest_still_records_a_plan_and_honors_parent_precedence() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("duhem.yml");
    let db = temp.path().join("evidence.db");
    std::fs::write(&target, "manifest_version: 1\nverifications: []\n").unwrap();
    for (flag, expected) in [(None, "from-env"), (Some("from-flag"), "from-flag")] {
        let mut command = run(&target, &db);
        command
            .env("DUHEM_SHARD", "2/3")
            .env("DUHEM_RUN_ID", "from-env");
        if let Some(flag) = flag {
            command.args(["--run-id", flag]);
        }
        let summary: Value = serde_json::from_str(&stdout(command.output().unwrap())).unwrap();
        let (parent, plan) = evidence(&db, &summary).await;
        assert_eq!(parent, expected);
        assert_eq!(
            (plan.index, plan.total, plan.source.as_str()),
            (2, 3, "DUHEM_SHARD")
        );
        assert!(plan.assignments.is_empty());
        assert_eq!(summary["verdict"], "pass");
    }
}

#[test]
fn single_leaf_native_only_shard_gates_its_result() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("leaf.yml");
    let db = temp.path().join("evidence.db");
    std::fs::write(
        &target,
        "verification: native sharding\ncriteria:\n  - id: AC\n    description: Independently passes\n    checks:\n      - id: body\n        assertions: ['true']\nnative:\n  - id: required-tool\n    command: [sh, -c, 'exit 3']\n",
    ).unwrap();
    let first: Value = serde_json::from_str(&stdout(
        run(&target, &db).args(["--shard", "1/2"]).output().unwrap(),
    ))
    .unwrap();
    assert_eq!(first["totals"]["total"], 1);
    assert!(first.get("native").is_none());
    let second = run(&target, &db).args(["--shard", "2/2"]).output().unwrap();
    assert!(!second.status.success());
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["totals"]["total"], 0);
    assert_eq!(second["native"][0]["id"], "required-tool");
    assert_eq!(second["verdict"], "fail");
}

#[tokio::test]
async fn gitlab_job_suffix_does_not_split_the_parent() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("duhem.yml");
    let db = temp.path().join("evidence.db");
    std::fs::write(&target, "manifest_version: 1\nverifications: []\n").unwrap();
    let mut parents = Vec::new();
    for (index, job) in [("1", "smoke 1/2"), ("2", "smoke 2/2"), ("2", "lint 2/2")] {
        let summary: Value = serde_json::from_str(&stdout(
            run(&target, &db)
                .env("CI_NODE_INDEX", index)
                .env("CI_NODE_TOTAL", "2")
                .env("CI_PIPELINE_ID", "one")
                .env("CI_JOB_NAME", job)
                .output()
                .unwrap(),
        ))
        .unwrap();
        let (parent, plan) = evidence(&db, &summary).await;
        assert_eq!(plan.source, "gitlab");
        parents.push(parent);
    }
    assert_eq!(parents[0], parents[1]);
    assert_ne!(parents[0], parents[2]);
}
