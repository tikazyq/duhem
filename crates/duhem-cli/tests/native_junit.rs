//! Real shell commands copy hand-authored producer fixtures into report paths.
#![cfg(unix)]
use duhem_evidence::{SqliteStore, Store};
use std::process::Command;

const BODY: &str = "criteria:\n  - id: AC\n    description: Independent body\n    checks:\n      - id: body\n        assertions: ['true']\n";

fn run(source: &str, shell: &str, fields: &str) -> (serde_json::Value, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("work")).unwrap();
    std::fs::write(tmp.path().join("work/source.xml"), source).unwrap();
    std::fs::write(tmp.path().join("leaf.yml"), format!("verification: junit\ninputs:\n  token: {{type: string, secret: true}}\nnative:\n  - id: tests\n    layer: unit\n    cwd: work\n    command: [sh, -c, '{shell}']\n    report: {{format: junit, path: result.xml}}\n    {fields}\n{BODY}")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_duhem"))
        .args([
            "run",
            "--reporter",
            "json",
            "--no-live",
            "--inputs",
            "token=native-secret-value",
            "--db",
        ])
        .arg(tmp.path().join("evidence.db"))
        .arg(tmp.path().join("leaf.yml"))
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(output.status.success(), json["verdict"] == "pass", "{json}");
    assert_eq!(json["criteria"][0]["verdict"], "pass");
    (json, tmp)
}

async fn blob(store: &SqliteStore, run: &serde_json::Value, field: &str) -> Vec<u8> {
    store
        .get_blob(run[field].as_str().unwrap())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn five_producers_have_real_counts_and_normalized_evidence() {
    for (source, counts, outcome) in [
        (
            include_str!("fixtures/native/nextest.xml"),
            [1, 0, 0, 1],
            "pass",
        ),
        (
            include_str!("fixtures/native/pytest.xml"),
            [1, 1, 1, 1],
            "fail",
        ),
        (
            include_str!("fixtures/native/jest-junit.xml"),
            [1, 1, 0, 0],
            "fail",
        ),
        (
            include_str!("fixtures/native/playwright.xml"),
            [1, 1, 0, 0],
            "fail",
        ),
        (
            include_str!("fixtures/native/go-junit-report.xml"),
            [2, 0, 0, 1],
            "pass",
        ),
    ] {
        let (json, tmp) = run(source, "cp source.xml result.xml", "");
        let native = &json["native"][0];
        assert_eq!(native["outcome"], outcome);
        assert_eq!(native["report_path"], "result.xml");
        assert_eq!(native["tests"], counts.iter().sum::<u32>());
        for (index, key) in ["passed", "failed", "errored", "skipped"]
            .iter()
            .enumerate()
        {
            assert_eq!(native["counts"][*key], counts[index]);
        }
        let layer = json["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["layer"] == "unit")
            .unwrap();
        assert_eq!(layer["passed"], counts[0]);
        assert_eq!(layer["failed"], counts[1]);
        if counts[2] > 0 {
            assert_eq!(layer["errored"], counts[2]);
        }
        if counts[3] > 0 {
            assert_eq!(layer["skipped"], counts[3]);
        }
        let store = SqliteStore::open(tmp.path().join("evidence.db"))
            .await
            .unwrap();
        assert_eq!(
            blob(&store, native, "report_blob_sha256").await,
            source.as_bytes()
        );
        let results: serde_json::Value =
            serde_json::from_slice(&blob(&store, native, "results_blob_sha256").await).unwrap();
        assert_eq!(
            results.as_array().unwrap().len(),
            counts.iter().sum::<u32>() as usize
        );
        assert_eq!(results[0]["id"].as_str().unwrap().len(), 64);
        let events = store
            .run_events(json["run_id"].as_str().unwrap())
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(&event.payload,
            duhem_evidence::EventPayload::NativeRunFinished { result }
                if serde_json::to_value(result).unwrap() == *native)));
        let terminal = Command::new(env!("CARGO_BIN_EXE_duhem"))
            .args([
                "run",
                "--no-live",
                "--inputs",
                "token=native-secret-value",
                "--db",
            ])
            .arg(tmp.path().join("terminal.db"))
            .arg(tmp.path().join("leaf.yml"))
            .output()
            .unwrap();
        let text = String::from_utf8(terminal.stdout).unwrap();
        assert!(
            text.contains(&format!(
                "{} passed, {} failed, {} errored, {} skipped",
                counts[0], counts[1], counts[2], counts[3]
            )),
            "{text}"
        );
        if source.contains("pytest") {
            assert_eq!(results[0]["file"], "tests/test_api.py");
            assert_eq!(results[0]["duration_ms"], 100);
            assert_eq!(
                results[1]["message"],
                "assert 1 < 0\nTraceback: expected & observed"
            );
        }
        if source.contains("jest tests") {
            assert_eq!(results[0]["name"], "renders \"hello\"");
            assert!(
                results[1]["message"]
                    .as_str()
                    .unwrap()
                    .contains("<valid> & got invalid")
            );
        }
    }
}

#[test]
fn command_status_and_output_still_gate_with_test_counts() {
    for (shell, fields) in [
        ("cp source.xml result.xml; exit 7", ""),
        (
            "cp source.xml result.xml; echo warning",
            "expect: {stdout: {not_matches: warning}}",
        ),
    ] {
        let (json, _tmp) = run(include_str!("fixtures/native/nextest.xml"), shell, fields);
        assert_eq!(json["native"][0]["outcome"], "fail");
        assert_eq!(json["native"][0]["counts"]["passed"], 1);
        let layer = json["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["layer"] == "unit")
            .unwrap();
        assert_eq!(layer["passed"], 1);
        assert_eq!(layer["failed"], 0);
    }
}

#[test]
fn missing_malformed_and_timeout_are_inconclusive() {
    for (source, shell, fields, outcome) in [
        (
            "<testsuite>",
            "cp source.xml result.xml",
            "",
            "inconclusive:environment_error",
        ),
        (
            "<unrelated/>",
            "cp source.xml result.xml",
            "",
            "inconclusive:environment_error",
        ),
        ("", "exit 0", "", "inconclusive:environment_error"),
        (
            "<testsuite/>",
            "sleep 1",
            "timeout: 10ms",
            "inconclusive:timeout",
        ),
        (
            "<testsuite><testcase name=\"bad\"><failure/></testcase></testsuite>",
            "cp source.xml result.xml; sleep 1",
            "timeout: 100ms",
            "inconclusive:timeout",
        ),
    ] {
        let (json, _tmp) = run(source, shell, fields);
        assert_eq!(json["native"][0]["outcome"], outcome);
    }
}

#[tokio::test]
async fn masking_happens_before_encoding_and_summary_failures_are_bounded() {
    let long = format!("native-secret-value {}", "界".repeat(3000));
    let cases: String = (0..12).map(|index| format!("<testcase classname=\"native-secret-value\" name=\"case {index}\" file=\"native-secret-value.py\"><failure message=\"native-secret-value\"><![CDATA[{long}]]></failure></testcase>")).collect();
    let source = format!("<testsuite>{cases}</testsuite>");
    let (json, tmp) = run(&source, "cp source.xml result.xml", "");
    let native = &json["native"][0];
    assert!(!json.to_string().contains("native-secret-value"));
    assert_eq!(native["counts"]["failed"], 12);
    let failures = native["failing_tests"].as_array().unwrap();
    assert_eq!(failures.len(), 10);
    assert_eq!(
        failures[0]["message"].as_str().unwrap().chars().count(),
        2048
    );
    let store = SqliteStore::open(tmp.path().join("evidence.db"))
        .await
        .unwrap();
    for field in ["report_blob_sha256", "results_blob_sha256"] {
        let content = String::from_utf8(blob(&store, native, field).await).unwrap();
        assert!(!content.contains("native-secret-value"));
        assert!(content.contains("[redacted:token]"));
    }
    let results: serde_json::Value =
        serde_json::from_slice(&blob(&store, native, "results_blob_sha256").await).unwrap();
    assert_eq!(results.as_array().unwrap().len(), 12);
    assert!(results[0]["message"].as_str().unwrap().chars().count() > 3000);
    assert_eq!(results[0]["classname"], "[redacted:token]");
    let (again, _tmp) = run(&source, "cp source.xml result.xml", "");
    assert_eq!(
        native["results_blob_sha256"],
        again["native"][0]["results_blob_sha256"]
    );
}
