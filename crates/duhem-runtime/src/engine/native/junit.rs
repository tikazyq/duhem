//! Normalize the testcase records shared by common JUnit producers.
use duhem_summary::{NativeTestResult, NativeTestStatus};
use sha2::{Digest, Sha256};

pub(super) fn parse(
    source: &str,
    native_id: &str,
    mask: impl Fn(&str) -> String,
) -> Result<Vec<NativeTestResult>, String> {
    let document = roxmltree::Document::parse(source).map_err(|error| error.to_string())?;
    let root = document.root_element();
    if !matches!(root.tag_name().name(), "testsuite" | "testsuites") {
        return Err("expected testsuite or testsuites root".into());
    }
    let mut results = Vec::new();
    for case in root
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
    {
        let classname = mask(case.attribute("classname").unwrap_or(""));
        let name = mask(case.attribute("name").ok_or("testcase missing name")?);
        let duration_ms = match case.attribute("time") {
            None => 0,
            Some(time) => {
                let seconds: f64 = time.parse().map_err(|_| "invalid testcase time")?;
                if !seconds.is_finite() || seconds < 0.0 || seconds * 1000.0 > u64::MAX as f64 {
                    return Err("invalid testcase time".into());
                }
                (seconds * 1000.0).round() as u64
            }
        };
        let mut status = NativeTestStatus::Passed;
        let mut messages = Vec::new();
        for record in case.children().filter(|node| node.is_element()) {
            let observed = match record.tag_name().name() {
                "error" => NativeTestStatus::Errored,
                "failure" => NativeTestStatus::Failed,
                "skipped" => NativeTestStatus::Skipped,
                _ => continue,
            };
            status = match (status, observed) {
                (NativeTestStatus::Errored, _) | (_, NativeTestStatus::Errored) => {
                    NativeTestStatus::Errored
                }
                (NativeTestStatus::Failed, _) | (_, NativeTestStatus::Failed) => {
                    NativeTestStatus::Failed
                }
                _ => NativeTestStatus::Skipped,
            };
            if let Some(message) = record.attribute("message") {
                messages.push(message.to_string());
            }
            let text: String = record
                .descendants()
                .filter(|node| node.is_text())
                .filter_map(|node| node.text())
                .collect();
            if !text.is_empty() {
                messages.push(text);
            }
        }
        // Hash an unambiguous tuple; mask each string before JSON encoding.
        let identity =
            serde_json::to_vec(&(mask(native_id), &classname, &name)).expect("identity serializes");
        results.push(NativeTestResult {
            id: hex::encode(Sha256::digest(identity)),
            classname,
            name,
            status,
            duration_ms,
            file: case.attribute("file").map(&mask),
            message: (!messages.is_empty()).then(|| mask(&messages.join("\n"))),
        });
    }
    Ok(results)
}
