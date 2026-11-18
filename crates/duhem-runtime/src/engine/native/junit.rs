//! Normalize the JUnit dialects emitted by native test runners.
use duhem_summary::{NativeTestResult, NativeTestStatus};
use roxmltree::{Document, Node};

pub(super) fn parse(xml: &str, native_id: &str) -> Result<Vec<NativeTestResult>, String> {
    let document = Document::parse(xml).map_err(|error| error.to_string())?;
    let root = document.root_element();
    if !is_suite(root) {
        return Err("expected a testsuite or testsuites root".into());
    }

    root.descendants()
        .filter(|node| {
            node.has_tag_name("testcase")
                && node
                    .parent()
                    .is_some_and(|suite| suite.has_tag_name("testsuite"))
                && std::iter::successors(node.parent(), |parent| parent.parent())
                    .take_while(|parent| parent.is_element())
                    .all(is_suite)
        })
        .map(|node| parse_case(node, native_id))
        .collect()
}

fn is_suite(node: Node<'_, '_>) -> bool {
    node.has_tag_name("testsuite") || node.has_tag_name("testsuites")
}

fn parse_case(node: Node<'_, '_>, native_id: &str) -> Result<NativeTestResult, String> {
    use NativeTestStatus::{Errored, Failed, Passed, Skipped};

    let name = node.attribute("name").ok_or("testcase has no name")?;
    let class = node.attribute("classname").unwrap_or_default();
    let duration_ms = node.attribute("time").map(parse_time).transpose()?;
    let mut status = Passed;
    let mut messages = Vec::new();
    for child in node.children().filter(Node::is_element) {
        match child.tag_name().name() {
            "error" => status = Errored,
            "failure" if status != Errored => status = Failed,
            "failure" => {}
            "skipped" if status == Passed => status = Skipped,
            "skipped" => {}
            _ => continue,
        }
        let body: String = child
            .descendants()
            .filter(Node::is_text)
            .filter_map(|text| text.text())
            .collect();
        if let Some(message) = child
            .attribute("message")
            .filter(|message| !message.is_empty() && *message != body.trim())
        {
            messages.push(message.to_owned());
        }
        if !body.trim().is_empty() {
            messages.push(body);
        }
    }

    Ok(NativeTestResult {
        id: format!("{native_id}::{class}::{name}"),
        status,
        duration_ms,
        file: node.attribute("file").map(str::to_owned),
        message: (!messages.is_empty()).then(|| messages.join("\n")),
    })
}

fn parse_time(time: &str) -> Result<u64, String> {
    let seconds = time.parse::<f64>().map_err(|_| "invalid testcase time")?;
    let millis = (seconds * 1_000.0).round();
    if !millis.is_finite() || seconds < 0.0 || millis >= u64::MAX as f64 {
        return Err("invalid testcase time".into());
    }
    Ok(millis as u64)
}
