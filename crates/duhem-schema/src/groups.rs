//! Inline groups share their enclosing step namespace and dispatch sequence.
use std::collections::{BTreeMap, BTreeSet};

use crate::source::{SourceMap, SourcePathSegment as S, check_path};
use crate::{PathRoot, Step, StepCondition, StepGroup, ValidationError, VerificationDefinition};

pub(crate) fn validate(v: &VerificationDefinition) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for block in crate::step_walk::step_blocks(v) {
        // Descend through groups in the same namespace. Loop bodies are
        // separate blocks and are visited independently by the walker.
        if block.group_body || (block.check.is_some() && block.path.len() > 5) {
            continue;
        }
        let mut scope = Scope::default();
        scope.collect(
            v,
            block.steps,
            &block.path,
            0,
            block.check.is_some(),
            &mut errors,
        );
        if !scope.has_groups {
            continue;
        }
        for (id, entries) in scope.ids {
            if entries.len() > 1 {
                push(
                    &mut errors,
                    &v.source_map,
                    &entries[1],
                    format!("duplicate step id `{id}` in group namespace"),
                );
            }
        }
        for (raw, path) in scope.references {
            reject_reference(&raw, &scope.labels, &v.source_map, &path, &mut errors);
        }
        if let Some(check) = block.check {
            let [_, S::Index(ci), _, S::Index(hi), _] = block.path.as_slice() else {
                unreachable!("check body path")
            };
            for (ai, assertion) in check.assertions.iter().enumerate() {
                assertion.walk_exprs(|expr| {
                    if let Some(id) = group_reference(&expr.raw, &scope.labels) {
                        errors.push(ValidationError::InvalidGroup {
                            message: reference_message(id),
                            location: v
                                .source_map
                                .assertion_location(*ci, *hi, ai, assertion, &expr.raw),
                        });
                    }
                });
            }
        }
    }
    errors
}

/// Individually valid groups can become too deep when their flow calls
/// are expanded. Loop bodies and registered cleanups have their own sequence.
pub(crate) fn validate_expanded_depth(v: &VerificationDefinition) -> Vec<ValidationError> {
    fn check(steps: &[Step], enclosing: usize) -> Option<ValidationError> {
        for step in steps {
            let mut depth = enclosing;
            for group in step.groups.iter().filter(|group| !group.label.is_empty()) {
                depth += 1;
                if depth > crate::flows::MAX_FLOW_DEPTH {
                    return Some(ValidationError::InvalidGroup {
                        message: format!(
                            "group `{}` exceeds group depth {} after flow expansion",
                            group.label,
                            crate::flows::MAX_FLOW_DEPTH
                        ),
                        location: group.location,
                    });
                }
            }
            if let Some(error) = check(&step.for_each_body, depth) {
                return Some(error);
            }
            for cleanup in &step.flow_cleanup {
                if let Some(error) = check(&cleanup.steps, 0) {
                    return Some(error);
                }
            }
        }
        None
    }
    crate::step_walk::step_blocks(v)
        .into_iter()
        .filter(|block| !block.nested)
        .filter_map(|block| check(block.steps, 0))
        .collect()
}

#[derive(Default)]
struct Scope<'a> {
    labels: BTreeSet<&'a str>,
    has_groups: bool,
    ids: BTreeMap<&'a str, Vec<Vec<S>>>,
    references: Vec<(String, Vec<S>)>,
}

impl<'a> Scope<'a> {
    fn collect(
        &mut self,
        v: &VerificationDefinition,
        steps: &'a [Step],
        path: &[S],
        depth: usize,
        check_body: bool,
        errors: &mut Vec<ValidationError>,
    ) {
        for (index, step) in steps.iter().enumerate() {
            let path = field(path, S::index(index));
            let body_path = field(&path, S::key("steps"));
            let group = step.for_each.is_none() && step.steps.is_some();
            self.has_groups |= group;
            if step.steps.is_none()
                && ["null", "Null", "NULL", "~", ""]
                    .iter()
                    .any(|raw| v.source_map.scalar_location(&body_path, raw).is_some())
            {
                push(
                    errors,
                    &v.source_map,
                    &body_path,
                    "`steps:` must be a step list".into(),
                );
            }
            if let Some(id) = step.id.as_deref() {
                self.ids
                    .entry(id)
                    .or_default()
                    .push(field(&path, S::key("id")));
                if group {
                    self.labels.insert(id);
                }
            }
            if let StepCondition::Expr(expr) = &step.condition {
                self.references
                    .push((expr.raw.clone(), field(&path, S::key("if"))));
            }
            if let Some(expr) = &step.for_each {
                self.references
                    .push((expr.raw.clone(), field(&path, S::key("for_each"))));
            }
            value_references(
                &step.with,
                &field(&path, S::key("with")),
                &mut self.references,
            );
            if group {
                let body = step.steps.as_deref().unwrap();
                for (present, key, message) in [
                    (
                        step.uses.is_some() || step.call.is_some(),
                        "steps",
                        "a group cannot combine `steps:` with `uses:` or `call:`",
                    ),
                    (
                        step.id.as_deref().is_none_or(|id| id.trim().is_empty()),
                        "id",
                        "a group requires a non-empty `id:`",
                    ),
                    (
                        body.is_empty(),
                        "steps",
                        "a group requires a non-empty `steps:` body",
                    ),
                    (
                        !matches!(step.with, serde_yml::Value::Null),
                        "with",
                        "`with:` is not valid on a group",
                    ),
                    (
                        !step.outputs.is_empty(),
                        "outputs",
                        "`outputs:` is not valid on a group",
                    ),
                    (
                        !step.secret_outputs.is_empty(),
                        "secret_outputs",
                        "`secret_outputs:` is not valid on a group",
                    ),
                    (
                        !step.needs.is_empty(),
                        "needs",
                        "`needs:` is not valid on a group",
                    ),
                    (step.max.is_some(), "max", "`max:` requires `for_each:`"),
                    (
                        step.as_binding.is_some(),
                        "as",
                        "`as:` requires `for_each:`",
                    ),
                ] {
                    let key_path = field(&path, S::key(key));
                    if present
                        || (matches!(key, "with" | "outputs" | "secret_outputs" | "needs")
                            && v.source_map.node_location(&key_path).is_some())
                    {
                        push(errors, &v.source_map, &key_path, message.into());
                    }
                }
                if depth >= crate::flows::MAX_FLOW_DEPTH {
                    push(
                        errors,
                        &v.source_map,
                        &body_path,
                        format!("group depth exceeds {}", crate::flows::MAX_FLOW_DEPTH),
                    );
                } else {
                    self.collect(v, body, &body_path, depth + 1, check_body, errors);
                }
            } else {
                if depth > 0 && step.for_each.is_none() {
                    let uses = step
                        .uses
                        .as_deref()
                        .is_some_and(|name| !name.trim().is_empty());
                    let call = step
                        .call
                        .as_deref()
                        .is_some_and(|name| !name.trim().is_empty());
                    if uses == call || step.max.is_some() || step.as_binding.is_some() {
                        push(errors, &v.source_map, &field(&path, S::key("uses")),
                            "a group body step must declare exactly one of `uses:` or `call:` and may use `max:` or `as:` only with `for_each:`".into());
                    }
                }
                if check_body && let Some(body) = &step.steps {
                    self.collect(v, body, &body_path, depth, check_body, errors);
                }
            }
        }
    }
}

fn field(path: &[S], segment: S) -> Vec<S> {
    let mut path = path.to_vec();
    path.push(segment);
    path
}

fn push(errors: &mut Vec<ValidationError>, source: &SourceMap, path: &[S], message: String) {
    errors.push(ValidationError::InvalidGroup {
        message,
        location: source
            .node_location(path)
            .or_else(|| source.node_location(&path[..path.len() - 1])),
    });
}

fn value_references(value: &serde_yml::Value, path: &[S], refs: &mut Vec<(String, Vec<S>)>) {
    match value {
        serde_yml::Value::String(raw) if crate::expr::is_expression(raw) => {
            refs.push((raw.clone(), path.to_vec()))
        }
        serde_yml::Value::Mapping(map) => {
            for (key, value) in map {
                if let Some(key) = key.as_str() {
                    value_references(value, &field(path, S::key(key)), refs);
                }
            }
        }
        serde_yml::Value::Sequence(items) => {
            for (index, value) in items.iter().enumerate() {
                value_references(value, &field(path, S::index(index)), refs);
            }
        }
        _ => {}
    }
}

fn group_reference<'a>(raw: &str, labels: &BTreeSet<&'a str>) -> Option<&'a str> {
    let mut found = None;
    crate::expr::parse(raw).ok()?.walk_paths(|path| {
        if path.root == PathRoot::Steps
            && let Some(id) = path.segments().first()
        {
            found = found.or_else(|| labels.get(id.as_str()).copied());
        }
    });
    found
}

fn reference_message(id: &str) -> String {
    format!("`$steps.{id}` names a group label, which has no outputs")
}

fn reject_reference(
    raw: &str,
    labels: &BTreeSet<&str>,
    source: &SourceMap,
    path: &[S],
    errors: &mut Vec<ValidationError>,
) {
    if let Some(id) = group_reference(raw, labels) {
        push(errors, source, path, reference_message(id));
    }
}

/// Flatten groups before expanding flows. Retain the original source marks
/// at the new action indices so existing validators still locate fields.
pub(crate) fn flatten(v: &mut VerificationDefinition) {
    let source = v.source_map.clone();
    let mut key = 0;
    let mut flatten = |steps: &mut Vec<Step>, path: Vec<S>| {
        if !has_group(steps) {
            return;
        }
        v.source_map.clear_step_nodes(&path);
        let mut flattener = Flatten {
            source: &source,
            target: &mut v.source_map,
            key: &mut key,
        };
        flattener.list(
            std::mem::take(steps),
            &path,
            &path,
            &[],
            None,
            &StepCondition::Success,
            steps,
        );
    };
    flatten(&mut v.setup, vec![S::key("setup")]);
    flatten(&mut v.teardown, vec![S::key("teardown")]);
    for (name, fixture) in &mut v.fixtures {
        flatten(
            &mut fixture.up,
            vec![S::key("fixtures"), S::key(name), S::key("up")],
        );
        flatten(
            &mut fixture.down,
            vec![S::key("fixtures"), S::key(name), S::key("down")],
        );
    }
    for (name, flow) in &mut v.flows {
        flatten(
            &mut flow.steps,
            vec![S::key("flows"), S::key(name), S::key("steps")],
        );
        flatten(
            &mut flow.cleanup,
            vec![S::key("flows"), S::key(name), S::key("cleanup")],
        );
    }
    for (ci, criterion) in v.criteria.iter_mut().enumerate() {
        let path = vec![S::key("criteria"), S::index(ci)];
        flatten(&mut criterion.setup, field(&path, S::key("setup")));
        flatten(&mut criterion.teardown, field(&path, S::key("teardown")));
        for (hi, check) in criterion.checks.iter_mut().enumerate() {
            flatten(&mut check.setup, check_path(ci, hi, "setup"));
            flatten(&mut check.steps, check_path(ci, hi, "steps"));
            flatten(&mut check.teardown, check_path(ci, hi, "teardown"));
        }
    }
}

pub(crate) fn has_group(steps: &[Step]) -> bool {
    steps.iter().any(|step| {
        step.steps
            .as_ref()
            .is_some_and(|body| step.for_each.is_none() || has_group(body))
    })
}

struct Flatten<'a> {
    source: &'a SourceMap,
    target: &'a mut SourceMap,
    key: &'a mut usize,
}

impl Flatten<'_> {
    #[allow(clippy::too_many_arguments)]
    fn list(
        &mut self,
        steps: Vec<Step>,
        from: &[S],
        to: &[S],
        groups: &[StepGroup],
        session: Option<&str>,
        default: &StepCondition,
        result: &mut Vec<Step>,
    ) {
        for (index, mut step) in steps.into_iter().enumerate() {
            let from = field(from, S::index(index));
            let to = field(to, S::index(result.len()));
            if step.condition.is_default() {
                step.condition = default.clone();
            }
            if step.for_each.is_none()
                && let Some(body) = step.steps.take()
            {
                let mut groups = groups.to_vec();
                let label = step.id.expect("validated group id");
                let location = self
                    .source
                    .node_location(&field(&from, S::key("if")))
                    .or_else(|| self.source.node_location(&field(&from, S::key("id"))));
                let inner_default = match step.condition {
                    StepCondition::Always | StepCondition::Failure => &step.condition,
                    _ => default,
                };
                groups.push(StepGroup {
                    label,
                    condition: step.condition.clone(),
                    key: *self.key,
                    invocation: None,
                    location,
                });
                *self.key += 1;
                self.list(
                    body,
                    &field(&from, S::key("steps")),
                    &to[..to.len() - 1],
                    &groups,
                    step.session.as_deref().or(session),
                    inner_default,
                    result,
                );
                continue;
            }
            self.target.copy_step_nodes(self.source, &from, &to);
            if let Some(body) = step.steps.take() {
                let mut flat = Vec::new();
                self.target.clear_step_nodes(&field(&to, S::key("steps")));
                self.list(
                    body,
                    &field(&from, S::key("steps")),
                    &field(&to, S::key("steps")),
                    &[],
                    step.session.as_deref().or(session),
                    default,
                    &mut flat,
                );
                step.steps = Some(flat);
            }
            if step.session.is_none()
                && (step.call.is_some()
                    || step
                        .uses
                        .as_deref()
                        .is_some_and(|uses| uses.starts_with("ui/")))
            {
                step.session = session.map(str::to_owned);
            }
            step.groups = groups.to_vec();
            result.push(step);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(setup: &str) -> String {
        format!(
            "verification: groups\nsetup:\n{setup}\ncriteria:\n  - id: AC-1\n    description: A claim\n    checks:\n      - id: AC-1.1\n        assertions: [\"true\"]\n"
        )
    }

    #[test]
    fn malformed_groups_are_located_in_direct_validation_and_loading() {
        for (setup, message, line) in [
            (
                "  - id: mixed\n    uses: cli/invoke\n    steps: [{ uses: cli/invoke }]",
                "cannot combine",
                5,
            ),
            (
                "  - id: mixed\n    call: flow\n    steps: [{ uses: cli/invoke }]",
                "cannot combine",
                5,
            ),
            (
                "  - id: payload\n    with: {}\n    steps: [{ uses: cli/invoke }]",
                "`with:` is not valid",
                4,
            ),
            (
                "  - id: payload\n    with: null\n    steps: [{ uses: cli/invoke }]",
                "`with:` is not valid",
                4,
            ),
            ("  - id: empty\n    steps: []", "non-empty `steps:`", 4),
            ("  - steps: [{ uses: cli/invoke }]", "non-empty `id:`", 3),
            (
                "  - id: bad\n    steps: [{ uses: cli/invoke }]\n    outputs: {}",
                "`outputs:` is not valid",
                5,
            ),
            ("  - id: bad\n    steps: null", "must be a step list", 4),
            (
                "  - id: a\n    steps: [{ id: same, uses: cli/invoke }]\n  - id: b\n    steps: [{ id: same, uses: cli/invoke }]",
                "duplicate step id `same`",
                6,
            ),
            (
                "  - id: a\n    steps: [{ uses: cli/invoke }]\n  - uses: cli/invoke\n    with: { command: [echo, $steps.a.outputs.any] }",
                "names a group label",
                6,
            ),
            (
                "  - id: a\n    steps:\n      - id: b\n        steps:\n          - id: c\n            steps:\n              - id: d\n                steps: [{ uses: cli/invoke }]",
                "group depth exceeds",
                10,
            ),
        ] {
            let yaml = definition(setup);
            let authored = VerificationDefinition::from_yaml_str(&yaml).unwrap();
            let errors = crate::validate(&authored).unwrap_err();
            let location = errors
                .iter()
                .find(|error| error.to_string().contains(message))
                .and_then(ValidationError::location)
                .expect("located validation error");
            assert_eq!(location.line, line, "{errors:?}");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("verification.yml");
            std::fs::write(&path, yaml).unwrap();
            let error = crate::load(&path).unwrap_err().to_string();
            assert!(
                error.contains(message) && error.contains(&format!("{line}:{}:", location.column)),
                "{error}"
            );
        }
    }

    #[test]
    fn assertion_cannot_reference_group_label() {
        let yaml = r#"
verification: groups
criteria:
  - id: AC-1
    description: A claim
    checks:
      - id: AC-1.1
        steps:
          - id: group
            steps: [{ uses: cli/invoke }]
        assertions: [$steps.group.outputs.code == 0]
"#;
        let authored = VerificationDefinition::from_yaml_str(yaml).unwrap();
        assert!(crate::validate(&authored).unwrap_err().iter().any(|error| {
            error.to_string().contains("names a group label")
                && error.location().is_some_and(|loc| loc.line == 11)
        }));
    }

    #[test]
    fn named_session_and_loop_bound_survive_flattening() {
        let yaml = r#"
verification: groups in a loop
inputs:
  rows: { type: array }
criteria:
  - id: AC-1
    description: A claim
    checks:
      - id: AC-1.1
        sessions: { primary: null, secondary: null }
        steps:
          - for_each: $inputs.rows
            max: 5
            steps:
              - id: browser
                session: primary
                steps:
                  - uses: ui/navigate
                  - uses: ui/assert-element
                    session: secondary
"#;
        let authored = VerificationDefinition::from_yaml_str(yaml).unwrap();
        assert_eq!(
            authored.criteria[0].checks[0].worst_case_step_count(),
            Ok(10)
        );
        crate::validate(&authored).unwrap();
        let mut loaded = authored;
        crate::flows::validate_and_expand(&mut loaded).unwrap();
        crate::validate(&loaded).unwrap();
        let check = &loaded.criteria[0].checks[0];
        assert_eq!(check.worst_case_step_count(), Ok(10));
        let body = &check.steps[0].for_each_body;
        assert_eq!(body[0].session.as_deref(), Some("primary"));
        assert_eq!(body[1].session.as_deref(), Some("secondary"));
    }

    #[test]
    fn flow_calls_cannot_exceed_the_group_depth_limit() {
        let yaml = r#"
verification: groups across calls
flows:
  prepare:
    steps:
      - id: first
        steps:
          - id: second
            steps: [{ uses: cli/invoke }]
setup:
  - id: setup_group
    steps:
      - id: inner
        steps: [{ call: prepare }]
criteria:
  - id: AC-1
    description: A claim
    checks:
      - id: AC-1.1
        assertions: ["true"]
"#;
        let mut authored = VerificationDefinition::from_yaml_str(yaml).unwrap();
        assert!(crate::validate(&authored).unwrap_err().iter().any(|error| {
            error.to_string().contains("after flow expansion") && error.location().is_some()
        }));
        assert!(
            crate::flows::validate_and_expand(&mut authored)
                .unwrap_err()
                .contains("after flow expansion")
        );
    }

    #[test]
    fn a_group_cannot_hide_a_nested_loop() {
        let yaml = definition(
            "  - for_each: $inputs.rows\n    max: 2\n    steps:\n      - id: group\n        steps:\n          - for_each: $inputs.rows\n            max: 2\n            uses: cli/invoke",
        );
        let authored = VerificationDefinition::from_yaml_str(&yaml).unwrap();
        let errors = crate::validate(&authored).unwrap_err();
        assert!(errors.iter().any(|error| matches!(error, ValidationError::ForEachDepthExceeded { location: Some(loc), .. } if loc.line == 8)), "{errors:?}");
    }
}
