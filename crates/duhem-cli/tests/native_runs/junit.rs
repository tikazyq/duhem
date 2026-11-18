use super::{BODY, Command, execute};
use duhem_evidence::{SqliteStore, Store, bundle::referenced_blobs};
use serde_json::{Value, json};

fn command(fixture: &str, suffix: &str, expect: &str) -> String {
    let fixture = format!(
        "{}/tests/fixtures/junit/{fixture}.xml",
        env!("CARGO_MANIFEST_DIR")
    );
    format!(
        "native:\n  - id: tool\n    layer: unit\n    command: [sh, -c, 'cp \"$JUNIT_FIXTURE\" report.xml; {suffix}']\n    env: {{JUNIT_FIXTURE: {}}}\n    report: {{format: junit, path: report.xml}}\n{expect}",
        json!(fixture),
    )
}

#[tokio::test]
async fn reads_five_junit_dialects_and_retains_evidence() {
    for (fixture, counts, failure, manifest) in [
        ("nextest", [2u64, 0, 0, 0], None, false),
        (
            "pytest",
            [1, 0, 1, 1],
            Some("fixture \"shared\" broke\nRuntimeError: expected <ready> & found <busy>"),
            true,
        ),
        (
            "jest-junit",
            [1, 1, 0, 0],
            Some("Expected <a> before <b> & received the reverse."),
            false,
        ),
        ("playwright", [1, 0, 0, 1], None, false),
        (
            "go-junit-report",
            [1, 1, 0, 0],
            Some("Failed\nstore_test.go:12: got \"busy\""),
            false,
        ),
    ] {
        let (success, summary, tmp) = execute(&command(fixture, "exit 0", ""), manifest);
        assert_eq!(success, failure.is_none(), "{fixture}: {summary}");
        let [passed, failed, errored, skipped] = counts;
        let run = &summary["native"][0];
        assert_eq!(
            run["counts"],
            json!({"passed": passed, "failed": failed, "errored": errored, "skipped": skipped})
        );
        assert_eq!(run["tests"], passed + failed + errored + skipped);
        assert_eq!(run["outcome"], if success { "pass" } else { "fail" });
        assert_eq!(run["report_path"], "report.xml");
        assert_eq!(run["failing_tests"][0]["message"].as_str(), failure);
        let unit = summary["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["layer"] == "unit")
            .unwrap();
        assert_eq!(unit["passed"], passed);
        assert_eq!(unit["failed"], failed);
        assert_eq!(unit["errored"].as_u64(), (errored != 0).then_some(errored));
        assert_eq!(unit["skipped"].as_u64(), (skipped != 0).then_some(skipped));
        let body = if manifest {
            &summary["runs"][0]
        } else {
            &summary
        };
        assert_eq!(body["criteria"][0]["verdict"], "pass");

        let store = SqliteStore::open(tmp.path().join("evidence.db"))
            .await
            .unwrap();
        let events = store
            .run_events(summary["run_id"].as_str().unwrap())
            .await
            .unwrap();
        let referenced = referenced_blobs(&events);
        for field in ["report_blob_sha256", "results_blob_sha256"] {
            assert!(
                referenced
                    .iter()
                    .any(|sha| sha == run[field].as_str().unwrap())
            );
        }
        let xml = get_blob(&store, run, "report_blob_sha256").await;
        assert_eq!(
            xml.as_bytes(),
            std::fs::read(tmp.path().join("report.xml")).unwrap()
        );
        let results: Value =
            serde_json::from_str(&get_blob(&store, run, "results_blob_sha256").await).unwrap();
        assert_eq!(
            results.as_array().unwrap().len() as u64,
            passed + failed + errored + skipped
        );
        if fixture == "nextest" {
            assert_eq!(
                results[0],
                json!({"id": "tool::example::integration::request_ok", "status": "passed", "duration_ms": 125})
            );
        }
        if fixture == "jest-junit" {
            assert_eq!(results[1]["file"], "src/search.test.js");
        }
        if fixture == "pytest" {
            assert_eq!(results[1]["status"], "errored");
            assert_eq!(results[2]["status"], "skipped");
        }
    }
}

#[test]
fn command_failures_do_not_change_test_counts() {
    for (suffix, expect) in [
        ("exit 9", ""),
        (
            "echo warning >&2",
            "    expect: {stderr: {not_matches: warning}}\n",
        ),
    ] {
        let (success, summary, _) = execute(&command("nextest", suffix, expect), false);
        assert!(!success, "{summary}");
        assert_eq!(summary["native"][0]["outcome"], "fail");
        assert_eq!(summary["native"][0]["counts"]["passed"], 2);
        assert_eq!(summary["layers"][1]["passed"], 2);
        assert_eq!(summary["layers"][1]["failed"], 0);
    }
}

#[test]
fn unavailable_report_is_inconclusive() {
    for (script, timeout, cause, has_xml) in [
        ("exit 1", "1s", "environment_error", false),
        (
            "printf \"<testsuite>\" > report.xml; exit 1",
            "1s",
            "environment_error",
            true,
        ),
        (
            "printf \"<root/>\" > report.xml",
            "1s",
            "environment_error",
            true,
        ),
        (
            "printf \"<testsuite/>\" > report.xml; exec sleep 30",
            "100ms",
            "timeout",
            false,
        ),
    ] {
        let (success, summary, _) = execute(
            &format!(
                "native:\n  - id: tool\n    command: [sh, -c, '{script}']\n    timeout: {timeout}\n    report: {{format: junit, path: report.xml}}\n"
            ),
            false,
        );
        assert!(!success);
        let run = &summary["native"][0];
        assert_eq!(run["outcome"], format!("inconclusive:{cause}"));
        assert_eq!(run["tests"], 0);
        assert_eq!(run["report_path"], "report.xml");
        assert!(run.get("counts").is_none());
        assert_eq!(run["report_blob_sha256"].is_string(), has_xml);
        assert!(run.get("results_blob_sha256").is_none());
    }
}

#[tokio::test]
async fn cwd_nested_reports_masking_and_failure_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("command-dir");
    std::fs::create_dir(&cwd).unwrap();
    let secret = "junit-&-private";
    let escaped = "junit-&amp;-private";
    let message = format!("{secret} {}", "é".repeat(2100));
    let mut xml = String::from("<testsuites><testsuites><testsuite tests=\"99\">");
    for n in 1..=11 {
        xml.push_str(&format!("<testcase classname=\"{escaped}\" name=\"case-{n}\" file=\"{escaped}.rs\"><failure><![CDATA[{message}]]></failure></testcase>"));
    }
    xml.push_str("</testsuite></testsuites></testsuites>");
    std::fs::write(cwd.join("fixture.xml"), xml).unwrap();
    let leaf = tmp.path().join("leaf.yml");
    std::fs::write(&leaf, format!("verification: junit\ninputs:\n  token: {{type: string, secret: true}}\nnative:\n  - id: tool\n    cwd: command-dir\n    command: [sh, -c, 'cp fixture.xml report.xml']\n    report: {{format: junit, path: report.xml}}\n{BODY}")).unwrap();
    let db = tmp.path().join("evidence.db");
    let run = |reporter: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_duhem"))
            .args([
                "run",
                "--reporter",
                reporter,
                "--no-live",
                "--inputs",
                &format!("token={secret}"),
                "--db",
            ])
            .arg(&db)
            .arg(&leaf)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains(secret));
        assert!(!stdout.contains(escaped));
        stdout
    };
    let summary: Value = serde_json::from_str(&run("json")).unwrap();
    let result = &summary["native"][0];
    assert_eq!(result["counts"]["failed"], 11);
    let failures = result["failing_tests"].as_array().unwrap();
    assert_eq!(failures.len(), 10);
    assert_eq!(
        failures[0]["message"].as_str().unwrap().chars().count(),
        2048
    );
    assert_eq!(failures[0]["id"], "tool::[redacted:token]::case-1");
    let terminal = run("default");
    assert!(terminal.contains("0 passed, 11 failed, 0 errored, 0 skipped"));
    assert!(terminal.contains("failure: tool::[redacted:token]::case-10"));
    assert!(!terminal.contains("case-11"));
    let store = SqliteStore::open(db).await.unwrap();
    let xml = get_blob(&store, result, "report_blob_sha256").await;
    assert!(!xml.contains(secret) && !xml.contains(escaped));
    assert!(xml.contains("[redacted:token]"));
    let results = get_blob(&store, result, "results_blob_sha256").await;
    assert!(!results.contains(secret) && !results.contains(escaped));
    let results: Value = serde_json::from_str(&results).unwrap();
    assert_eq!(results.as_array().unwrap().len(), 11);
    assert_eq!(results[10]["file"], "[redacted:token].rs");
    assert_eq!(
        results[10]["message"],
        format!("[redacted:token] {}", "é".repeat(2100))
    );
}

#[test]
fn accepts_a_bare_suite() {
    let (success, summary, _) = execute(
        "native:\n  - id: tool\n    command: [sh, -c, 'printf \"<testsuite><testcase name=\\\"test\\\"/></testsuite>\" > report.xml']\n    report: {format: junit, path: report.xml}\n",
        false,
    );
    assert!(success, "{summary}");
    assert_eq!(summary["native"][0]["counts"]["passed"], 1);
}

async fn get_blob(store: &SqliteStore, run: &Value, field: &str) -> String {
    let sha = run[field].as_str().unwrap();
    assert_eq!(sha.len(), 64);
    String::from_utf8(store.get_blob(sha).await.unwrap().unwrap()).unwrap()
}
