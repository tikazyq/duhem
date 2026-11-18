//! Static collection-window validation (#663 / #603).
use crate::source::SourcePathSegment as S;
use crate::{DurationSpec, ValidationError, VerificationDefinition};

pub(crate) fn validate(v: &VerificationDefinition, errors: &mut Vec<ValidationError>) {
    for block in crate::step_walk::step_blocks(v) {
        for (index, step) in block.steps.iter().enumerate() {
            if step.uses.as_deref() != Some("api/observe") {
                continue;
            }
            let Some(window) = step.with.get("window") else {
                continue;
            };
            let mut report = |field: &str, message: &str| {
                let mut path = block.path.clone();
                path.extend([
                    S::index(index),
                    S::key("with"),
                    S::key("window"),
                    S::key(field),
                ]);
                let location = v
                    .source_map
                    .node_location(&path)
                    .or_else(|| block.location(v, index, "with"));
                errors.push(ValidationError::InvalidObserveWindow {
                    site: block.step_site(index),
                    message: message.into(),
                    location,
                });
            };
            let anchor = window.get("from_step").and_then(serde_yml::Value::as_str);
            if anchor.is_none_or(str::is_empty) {
                report("from_step", "from_step must name an earlier step");
            } else if block.site.starts_with("flow `")
                || step
                    .flow
                    .as_ref()
                    .is_some_and(|flow| flow.name != "for_each")
            {
                report("from_step", "from_step is not supported inside a flow body");
            } else if block.check.is_none() {
                report("from_step", "collection windows require a check body");
            } else if !block.steps[..index]
                .iter()
                .any(|s| s.id.as_deref() == anchor)
            {
                report(
                    "from_step",
                    "anchor must be an earlier step in the same block",
                );
            }
            if !matches!(
                window.get("edge").and_then(serde_yml::Value::as_str),
                Some("start" | "finish")
            ) {
                report("edge", "edge must be start or finish");
            }
            let duration = window
                .get("duration")
                .cloned()
                .and_then(|value| serde_yml::from_value::<DurationSpec>(value).ok());
            if duration.is_none_or(|d| d.0.is_zero()) {
                report(
                    "duration",
                    "duration must be a positive integer in milliseconds or a duration with ms/s/m suffix",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(window: &str, nested: bool) -> VerificationDefinition {
        let steps = format!(
            "- id: anchor\n  uses: ui/wait\n  with: {{ duration: 1 }}\n- id: observed\n  uses: api/observe\n  with:\n    url_pattern: 're:.*'\n    window: {window}\n"
        );
        let steps = if nested {
            format!(
                "- for_each: '$inputs.items'\n  as: item\n  max: 1\n  steps:\n{}",
                steps
                    .lines()
                    .map(|l| format!("    {l}\n"))
                    .collect::<String>()
            )
        } else {
            steps
        };
        VerificationDefinition::from_yaml_str(&format!("verification: windows\ninputs:\n  items: {{ type: array }}\ncriteria:\n  - id: AC-1\n    description: Requests are observed.\n    checks:\n      - id: AC-1.1\n        assertions: ['true']\n        steps:\n{}", steps.lines().map(|l| format!("          {l}\n")).collect::<String>())).unwrap()
    }

    #[test]
    fn rejects_bad_fields_with_locations_including_loop_bodies() {
        for window in [
            "{ edge: start, duration: 1s }",
            "{ from_step: missing, edge: start, duration: 1s }",
            "{ from_step: observed, edge: start, duration: 1s }",
            "{ from_step: anchor, edge: unknown, duration: 1s }",
            "{ from_step: anchor, edge: start, duration: 0 }",
            "{ from_step: anchor, edge: start, duration: -1 }",
            "{ from_step: anchor, edge: start, duration: forever }",
            "{ from_step: anchor, edge: start, duration: 0s }",
            "{ from_step: anchor, edge: start, duration: -1s }",
            "{ from_step: anchor, edge: start, duration: 1.5 }",
            "{ from_step: anchor, edge: start }",
        ] {
            for nested in [false, true] {
                let v = definition(window, nested);
                let mut errors = Vec::new();
                validate(&v, &mut errors);
                assert!(!errors.is_empty(), "{window}");
                assert!(errors.iter().all(|e| e.location().is_some()), "{errors:?}");
            }
        }
    }

    #[test]
    fn accepts_earlier_anchor_and_rejects_flow_body() {
        let mut v = definition("{ from_step: anchor, edge: finish, duration: 1s }", false);
        let mut errors = Vec::new();
        validate(&v, &mut errors);
        assert!(errors.is_empty());
        let mut nested = definition("{ from_step: anchor, edge: start, duration: 1s }", true);
        crate::flows::validate_and_expand(&mut nested).unwrap();
        validate(&nested, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        let body = v.criteria[0].checks[0].steps.clone();
        v.flows
            .insert("sample".into(), serde_yml::from_str("steps: []").unwrap());
        v.flows.get_mut("sample").unwrap().steps = body;
        validate(&v, &mut errors);
        assert!(errors.iter().any(|e| e.to_string().contains("flow body")));
    }
}
