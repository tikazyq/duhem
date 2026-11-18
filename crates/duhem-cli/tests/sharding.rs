//! Shards execute real page-free checks and native child commands.
#![cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Output};

use duhem_evidence::{EventPayload, RunOrigin, SqliteStore, Store};

fn command(path: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_duhem"));
    cmd.args([
        "run",
        "--no-env-file",
        "--no-live",
        "--reporter",
        "json",
        "--db",
    ])
    .arg(db)
    .arg(path);
    for key in [
        "DUHEM_SHARD",
        "DUHEM_RUN_ID",
        "CI_NODE_INDEX",
        "CI_NODE_TOTAL",
        "BUILDKITE_PARALLEL_JOB",
        "BUILDKITE_PARALLEL_JOB_COUNT",
        "CIRCLE_NODE_INDEX",
        "CIRCLE_NODE_TOTAL",
        "CI_PIPELINE_ID",
        "CI_JOB_NAME",
        "BUILDKITE_BUILD_ID",
        "BUILDKITE_STEP_ID",
        "CIRCLE_WORKFLOW_ID",
        "CIRCLE_JOB",
        "DUHEM_PARENT_RUN_ID",
        "DUHEM_PARENT_DB_PATH",
    ] {
        cmd.env_remove(key);
    }
    cmd
}

fn success(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn fixture(root: &Path) -> std::path::PathBuf {
    for name in ["alpha", "beta"] {
        std::fs::write(
            root.join(format!("{name}.yml")),
            format!(
                r#"verification: {name}
native:
  - id: {name}-native
    command: [sh, -c, 'echo {name} >> native.log']
criteria:
  - id: AC-1
    description: first whole criterion
    checks:
      - {{id: one, assertions: ['true']}}
      - {{id: two, assertions: ['true']}}
  - id: AC-2
    description: second whole criterion
    checks: [{{id: three, assertions: ['true']}}]
"#
            ),
        )
        .unwrap();
    }
    let manifest = root.join("duhem.yml");
    std::fs::write(&manifest, "manifest_version: 1\nnative:\n  - {id: suite, command: [sh, -c, 'echo suite >> native.log']}\nverifications: [{path: alpha.yml}, {path: beta.yml}]\n").unwrap();
    manifest
}

#[tokio::test]
async fn shards_are_balanced_disjoint_complete_and_record_the_same_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut observed = BTreeSet::new();
    let mut counts = Vec::new();
    let mut digest = None;
    let mut parent = None;
    let mut run_ids = BTreeSet::new();
    for index in 1..=3 {
        let db = tmp.path().join(format!("{index}.db"));
        success(
            command(&path, &db)
                .args(["--shard", &format!("{index}/3")])
                .output()
                .unwrap(),
        );
        let store = SqliteStore::open_read_only(&db).await.unwrap();
        let runs = store.list_runs().await.unwrap();
        let shard = runs.iter().find(|run| run.shard.is_some()).unwrap();
        assert!(run_ids.insert(shard.run_id.clone()));
        assert_eq!(shard.lineage.origin, Some(RunOrigin::Shard));
        let plan = shard.shard.as_ref().unwrap();
        assert_eq!(plan.index, index);
        assert_eq!(plan.source, "flag");
        assert_eq!(
            shard.lineage.parent_run_id.as_ref(),
            Some(&plan.parent_run_id)
        );
        if let Some(digest) = &digest {
            assert_eq!(digest, &plan.plan_digest);
        }
        if let Some(parent) = &parent {
            assert_eq!(parent, &plan.parent_run_id);
        }
        digest = Some(plan.plan_digest.clone());
        parent = Some(plan.parent_run_id.clone());
        let expected: BTreeSet<_> = plan
            .units
            .iter()
            .filter(|unit| unit.shard == index)
            .map(|unit| {
                (
                    unit.verification.clone(),
                    unit.kind.clone(),
                    unit.id.clone(),
                )
            })
            .collect();
        let mut actual = BTreeSet::new();
        for run in &runs {
            let events = store.run_events(&run.run_id).await.unwrap();
            if run.shard.is_some() {
                assert!(events.iter().any(|event| matches!(&event.payload,
                    EventPayload::RunStarted { shard: Some(recorded), origin: Some(RunOrigin::Shard), .. } if recorded == plan)));
            }
            let leaf = (run.shard.is_none()).then(|| {
                Path::new(&run.verification)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            });
            let mut checks = BTreeMap::<String, usize>::new();
            for event in events {
                match event.payload {
                    EventPayload::CriterionFinished { criterion_id, .. } => {
                        actual.insert((leaf.clone(), "criterion".into(), criterion_id));
                    }
                    EventPayload::CheckFinished {
                        criterion_id: Some(criterion),
                        ..
                    } => {
                        *checks.entry(criterion).or_default() += 1;
                    }
                    EventPayload::NativeRunFinished { result } => {
                        actual.insert((leaf.clone(), "native".into(), result.id));
                    }
                    _ => {}
                }
            }
            for (criterion, count) in checks {
                assert_eq!(count, if criterion == "AC-1" { 2 } else { 1 });
            }
        }
        assert_eq!(actual, expected);
        counts.push(actual.len());
        for unit in actual {
            assert!(observed.insert(unit), "unit executed twice");
        }
    }
    assert_eq!(observed.len(), 7);
    assert!(counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1);
    let log = std::fs::read_to_string(tmp.path().join("native.log")).unwrap();
    assert_eq!(log.lines().count(), 3);
}

#[tokio::test]
async fn leaf_criteria_and_filtered_native_work_are_partitioned_before_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let path = tmp.path().join("alpha.yml");
    for index in 1..=2 {
        let db = tmp.path().join(format!("leaf{index}.db"));
        success(
            command(&path, &db)
                .args([
                    "--shard",
                    &format!("{index}/2"),
                    "--filter",
                    "AC-1",
                    "--filter",
                    "native:alpha-native",
                ])
                .output()
                .unwrap(),
        );
        let store = SqliteStore::open_read_only(&db).await.unwrap();
        let plan = store
            .list_runs()
            .await
            .unwrap()
            .into_iter()
            .find_map(|run| run.shard)
            .unwrap();
        assert_eq!(plan.units.len(), 2);
        assert!(plan.units.iter().all(|unit| unit.id != "AC-2"));
        let events = store.list_runs().await.unwrap();
        let mut check_count = 0;
        for run in events {
            check_count += store
                .run_events(&run.run_id)
                .await
                .unwrap()
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::CheckFinished { .. }))
                .count();
        }
        assert_eq!(
            check_count,
            if plan
                .units
                .iter()
                .any(|unit| unit.kind == "criterion" && unit.shard == index)
            {
                2
            } else {
                0
            }
        );
    }
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("native.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn invalid_shards_fail_before_creating_a_database_or_running_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    for raw in ["0/2", "1/0", "3/2", "a/2", "1/a", "1", "1/2/3", "-1/2"] {
        let db = tmp.path().join("invalid.db");
        let output = command(&path, &db)
            .arg(format!("--shard={raw}"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{raw}");
        assert!(!db.exists(), "{raw}");
        assert!(!tmp.path().join("native.log").exists(), "{raw}");
    }
}

#[tokio::test]
async fn flags_generic_environment_and_ci_have_ordered_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let cases = [
        ("gitlab", "CI_NODE_INDEX", "2", "CI_NODE_TOTAL", "3", 2),
        (
            "buildkite",
            "BUILDKITE_PARALLEL_JOB",
            "1",
            "BUILDKITE_PARALLEL_JOB_COUNT",
            "3",
            2,
        ),
        (
            "circleci",
            "CIRCLE_NODE_INDEX",
            "0",
            "CIRCLE_NODE_TOTAL",
            "3",
            1,
        ),
    ];
    for (source, index_key, index, total_key, total, expected) in cases {
        let db = tmp.path().join(format!("{source}.db"));
        success(
            command(&path, &db)
                .env(index_key, index)
                .env(total_key, total)
                .output()
                .unwrap(),
        );
        let store = SqliteStore::open_read_only(db).await.unwrap();
        let plan = store
            .list_runs()
            .await
            .unwrap()
            .into_iter()
            .find_map(|run| run.shard)
            .unwrap();
        assert_eq!(plan.source, source);
        assert_eq!(plan.index, expected);
        assert_eq!(plan.total, 3);
    }
    for explicit in [false, true] {
        let db = tmp.path().join(format!("precedence{explicit}.db"));
        let mut cmd = command(&path, &db);
        cmd.env("DUHEM_SHARD", if explicit { "bad" } else { "2/3" })
            .env("DUHEM_RUN_ID", "generic-parent")
            .env("CI_NODE_INDEX", "bad")
            .env("CI_NODE_TOTAL", "bad");
        if explicit {
            cmd.args(["--shard", "1/3", "--run-id", "flag-parent"]);
        }
        success(cmd.output().unwrap());
        let store = SqliteStore::open_read_only(db).await.unwrap();
        let plan = store
            .list_runs()
            .await
            .unwrap()
            .into_iter()
            .find_map(|run| run.shard)
            .unwrap();
        assert_eq!(plan.source, if explicit { "flag" } else { "env" });
        assert_eq!(
            plan.parent_run_id,
            if explicit {
                "flag-parent"
            } else {
                "generic-parent"
            }
        );
    }
}

#[tokio::test]
async fn empty_manifests_and_idle_shards_still_record_plans() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("duhem.yml");
    std::fs::write(&path, "manifest_version: 1\nverifications: []\n").unwrap();
    let db = tmp.path().join("empty.db");
    success(
        command(&path, &db)
            .args(["--shard", "4/4"])
            .output()
            .unwrap(),
    );
    let store = SqliteStore::open_read_only(db).await.unwrap();
    let runs = store.list_runs().await.unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].shard.as_ref().unwrap().units.is_empty());
    fixture(tmp.path());
    let db = tmp.path().join("idle.db");
    success(
        command(&path, &db)
            .args(["--shard", "8/8"])
            .output()
            .unwrap(),
    );
    assert!(!tmp.path().join("native.log").exists());
    let store = SqliteStore::open_read_only(db).await.unwrap();
    assert_eq!(store.list_runs().await.unwrap().len(), 1);
}

#[tokio::test]
async fn gitlab_parent_is_shared_across_nodes_and_separates_jobs_and_plans() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let mut parents = Vec::new();
    for (ordinal, (index, job, filter)) in [
        ("1", "verify 1/2", None),
        ("2", "verify 2/2", None),
        ("1", "other 1/2", None),
        ("1", "verify 1/2", Some("AC-1")),
    ]
    .into_iter()
    .enumerate()
    {
        let db = tmp.path().join(format!("identity{ordinal}.db"));
        let mut cmd = command(&path, &db);
        cmd.env("CI_NODE_INDEX", index)
            .env("CI_NODE_TOTAL", "2")
            .env("CI_PIPELINE_ID", "pipeline-123")
            .env("CI_JOB_NAME", job);
        if let Some(filter) = filter {
            cmd.args(["--filter", filter]);
        }
        success(cmd.output().unwrap());
        let store = SqliteStore::open_read_only(db).await.unwrap();
        parents.push(
            store
                .list_runs()
                .await
                .unwrap()
                .into_iter()
                .find_map(|run| run.shard)
                .unwrap()
                .parent_run_id,
        );
    }
    assert_eq!(parents[0], parents[1]);
    assert_ne!(parents[0], parents[2]);
    assert_ne!(parents[0], parents[3]);
}

#[test]
fn malformed_environment_and_incomplete_ci_pairs_fail_before_store_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    for (key, value) in [
        ("DUHEM_SHARD", "wat"),
        ("CI_NODE_INDEX", "1"),
        ("BUILDKITE_PARALLEL_JOB_COUNT", "2"),
        ("CIRCLE_NODE_TOTAL", "0"),
    ] {
        let db = tmp.path().join("invalid-env.db");
        let output = command(&path, &db).env(key, value).output().unwrap();
        assert!(!output.status.success(), "{key}");
        assert!(!db.exists(), "{key}");
        assert!(!tmp.path().join("native.log").exists(), "{key}");
    }
}

#[tokio::test]
async fn rerunning_a_shard_mints_fresh_execution_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let path = fixture(tmp.path());
    let db = tmp.path().join("repeat.db");
    for _ in 0..2 {
        success(
            command(&path, &db)
                .args(["--shard", "1/2", "--run-id", "shared-parent"])
                .output()
                .unwrap(),
        );
    }
    let store = SqliteStore::open_read_only(db).await.unwrap();
    let shards: Vec<_> = store
        .list_runs()
        .await
        .unwrap()
        .into_iter()
        .filter(|run| run.shard.is_some())
        .collect();
    assert_eq!(shards.len(), 2);
    assert_ne!(shards[0].run_id, shards[1].run_id);
    for run in shards {
        assert_eq!(run.shard.unwrap().parent_run_id, "shared-parent");
    }
}

#[test]
fn shard_lifecycle_runs_only_where_work_is_assigned() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let up = tmp.path().join("up.sh");
    std::fs::write(&up, "#!/bin/sh\necho up >> lifecycle.log\n").unwrap();
    std::fs::set_permissions(&up, std::fs::Permissions::from_mode(0o755)).unwrap();
    let down = tmp.path().join("down.sh");
    std::fs::write(&down, "#!/bin/sh\necho down >> lifecycle.log\n").unwrap();
    std::fs::set_permissions(&down, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = tmp.path().join("duhem.yml");
    std::fs::write(&path, "manifest_version: 1\nprovision: {up: ./up.sh, down: ./down.sh}\nverifications: [{path: alpha.yml}]\n").unwrap();
    for index in 1..=4 {
        let db = tmp.path().join(format!("lifecycle{index}.db"));
        success(
            command(&path, &db)
                .args(["--shard", &format!("{index}/4")])
                .output()
                .unwrap(),
        );
    }
    let log = std::fs::read_to_string(tmp.path().join("lifecycle.log")).unwrap();
    assert_eq!(log, "up\ndown\nup\ndown\nup\ndown\n");
}

#[test]
fn unsharded_dry_run_is_unchanged_and_sharded_dry_run_is_disjoint() {
    let tmp = tempfile::tempdir().unwrap();
    fixture(tmp.path());
    let path = tmp.path().join("alpha.yml");
    let db = tmp.path().join("dry.db");
    let output = command(&path, &db).arg("--dry-run").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "WOULD RUN: AC-1::one\nWOULD RUN: AC-1::two\nWOULD RUN: AC-2::three\nWOULD RUN NATIVE: alpha-native\nRESOLVED INPUT: (none)\n"
    );
    let mut lines = BTreeSet::new();
    for index in 1..=2 {
        let output = command(&path, &db)
            .args(["--dry-run", "--shard", &format!("{index}/2")])
            .output()
            .unwrap();
        assert!(output.status.success());
        for line in String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("WOULD RUN"))
        {
            assert!(lines.insert(line.to_owned()));
        }
    }
    assert_eq!(lines.len(), 4);
    assert!(!db.exists());
}
