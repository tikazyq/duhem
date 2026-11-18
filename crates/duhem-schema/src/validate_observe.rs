//! Validate literal request-window boundaries in their execution scope.

use std::collections::BTreeSet;

use crate::source::SourcePathSegment as S;
use crate::{DurationSpec, Step, ValidationError, VerificationDefinition};

pub(crate) fn validate(v: &VerificationDefinition, errors: &mut Vec<ValidationError>) {
    for block in crate::step_walk::step_blocks(v)
        .into_iter()
        .filter(|b| !b.nested)
    {
        let scope = Scope {
            v,
            site: &block.site,
            check: block.check.is_some(),
            flow: block.path.first() == Some(&S::key("flows")),
        };
        scope.steps(block.steps, &block.path, &mut BTreeSet::new(), errors);
    }
}

struct Scope<'a> {
    v: &'a VerificationDefinition,
    site: &'a str,
    check: bool,
    flow: bool,
}

impl Scope<'_> {
    fn steps<'a>(
        &self,
        steps: &'a [Step],
        block_path: &[S],
        preceding: &mut BTreeSet<&'a str>,
        errors: &mut Vec<ValidationError>,
    ) {
        for (index, step) in steps.iter().enumerate() {
            // The authored flow is checked at its definition, including unused ones.
            if step.flow.is_some() {
                continue;
            }
            let mut path = match block_path {
                [
                    S::Key(_),
                    S::Index(ci),
                    S::Key(_),
                    S::Index(hi),
                    S::Key(field),
                ] if field == "steps" && self.check => {
                    self.v.source_map.check_step_path(*ci, *hi, index)
                }
                _ => {
                    let mut path = block_path.to_vec();
                    path.push(S::index(index));
                    path
                }
            };
            if step.uses_name() == "api/observe"
                && let Some(window) = step.with.get("window").filter(|value| !value.is_null())
            {
                self.window(window, &path, preceding, errors);
            }
            if let Some(body) = &step.steps {
                path.push(S::key("steps"));
                if step.for_each.is_some() {
                    self.steps(body, &path, &mut preceding.clone(), errors);
                } else {
                    self.steps(body, &path, preceding, errors);
                }
            } else if step.for_each.is_none()
                && step.call.is_none()
                && let Some(id) = step.id.as_deref()
            {
                preceding.insert(id);
            }
        }
    }

    fn window(
        &self,
        window: &serde_yml::Value,
        path: &[S],
        preceding: &BTreeSet<&str>,
        errors: &mut Vec<ValidationError>,
    ) {
        let mut base = path.to_vec();
        base.extend([S::key("with"), S::key("window")]);
        let mut error = |field: &'static str, message| {
            let mut path = base.clone();
            if let Some(key) = field.strip_prefix('.') {
                path.push(S::key(key));
            }
            errors.push(ValidationError::InvalidObserveWindow {
                site: self.site.into(),
                field,
                message,
                location: self
                    .v
                    .source_map
                    .node_location(&path)
                    .or_else(|| self.v.source_map.node_location(&base)),
            });
        };
        if self.flow {
            error(".from_step", "is not supported inside a flow body");
            return;
        }
        if !self.check {
            error("", "is only supported in check steps");
            return;
        }
        if !window.is_mapping() {
            error("", "must be a mapping with from_step, edge and duration");
            return;
        }
        match window.get("from_step").and_then(serde_yml::Value::as_str) {
            None | Some("") => error(".from_step", "requires a step id"),
            Some(id) if !preceding.contains(id) => {
                error(".from_step", "must name an earlier step in scope")
            }
            Some(_) => {}
        }
        if !matches!(
            window.get("edge").and_then(serde_yml::Value::as_str),
            Some("start" | "finish")
        ) {
            error(".edge", "must be `start` or `finish`");
        }
        if !window.get("duration").is_some_and(|duration| {
            serde_yml::from_value::<DurationSpec>(duration.clone())
                .is_ok_and(|duration| !duration.0.is_zero())
        }) {
            error(
                ".duration",
                "must be a positive duration (e.g. `200ms`, `2s`, or integer milliseconds)",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(window: &str, nested: bool) -> VerificationDefinition {
        let steps = format!(
            "          - id: anchor\n            uses: ui/navigate\n          - id: observe\n            uses: api/observe\n            with:\n              window: {window}\n          - id: later\n            uses: ui/navigate\n"
        );
        let steps = if nested {
            let body: String = steps.lines().map(|line| format!("    {line}\n")).collect();
            format!(
                "          - for_each: $inputs.items\n            as: item\n            max: 1\n            steps:\n{body}"
            )
        } else {
            steps
        };
        VerificationDefinition::from_yaml_str(&format!(
            "verification: windows\ninputs:\n  items: {{type: array}}\ncriteria:\n  - id: AC-1\n    description: windows\n    checks:\n      - id: AC-1.1\n        assertions: ['true']\n        steps:\n{steps}"
        )).unwrap()
    }

    #[test]
    fn invalid_fields_have_locations_in_check_and_loop() {
        for (window, field) in [
            ("{edge: start, duration: 1s}", ".from_step"),
            (
                "{from_step: absent, edge: start, duration: 1s}",
                ".from_step",
            ),
            (
                "{from_step: later, edge: start, duration: 1s}",
                ".from_step",
            ),
            (
                "{from_step: observe, edge: start, duration: 1s}",
                ".from_step",
            ),
            ("{from_step: anchor, edge: middle, duration: 1s}", ".edge"),
            (
                "{from_step: anchor, edge: start, duration: 0ms}",
                ".duration",
            ),
            (
                "{from_step: anchor, edge: start, duration: -1}",
                ".duration",
            ),
            (
                "{from_step: anchor, edge: start, duration: -1s}",
                ".duration",
            ),
            (
                "{from_step: anchor, edge: start, duration: 2weeks}",
                ".duration",
            ),
        ] {
            for nested in [false, true] {
                let mut def = definition(window, nested);
                crate::flows::validate_and_expand(&mut def).unwrap();
                let errors = crate::validate(&def).unwrap_err();
                assert!(errors.iter().any(|e| matches!(e,
                    ValidationError::InvalidObserveWindow { field: actual, location: Some(at), .. }
                    if *actual == field && at.line == if nested { 20 } else { 16 }
                )), "{window}, nested={nested}: {errors:?}");
            }
        }
    }

    #[test]
    fn permits_earlier_step_in_loop() {
        let mut def = definition("{from_step: anchor, edge: finish, duration: 4}", true);
        crate::flows::validate_and_expand(&mut def).unwrap();
        crate::validate(&def).unwrap();
    }

    #[test]
    fn rejects_anchor_inside_unused_flow_at_definition() {
        let def = VerificationDefinition::from_yaml_str(
            r#"verification: windows
flows:
  traffic:
    steps:
      - id: anchor
        uses: ui/navigate
      - uses: api/observe
        with:
          window: {from_step: anchor, edge: start, duration: 1s}
criteria: []
"#,
        )
        .unwrap();
        let errors = crate::validate(&def).unwrap_err();
        assert!(errors.iter().any(|e| matches!(e,
            ValidationError::InvalidObserveWindow { field: ".from_step", location: Some(at), .. } if at.line == 9
        )), "{errors:?}");
    }
}
