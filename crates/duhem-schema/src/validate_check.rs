//! Check references and assertions, including seed timing.
use super::*;

pub(super) fn expand_condition_checks(
    v: &VerificationDefinition,
) -> Option<VerificationDefinition> {
    let needs_expansion = v.criteria.iter().flat_map(|c| &c.checks).any(|check| {
        let mut authored = check.steps.iter().flat_map(|step| {
            std::iter::once(step).chain(
                step.steps
                    .iter()
                    .flatten()
                    .filter(|_| step.for_each_body.is_empty()),
            )
        });
        authored
            .clone()
            .any(|step| step.call.is_some() && step.for_each_body.is_empty())
            && authored.any(|step| matches!(step.condition, StepCondition::Expr(_)))
    });
    if !needs_expansion {
        return None;
    }
    let mut expanded = v.clone();
    crate::flows::validate_and_expand(&mut expanded).ok()?;
    Some(expanded)
}

pub(super) fn validate_check(
    c: &Criterion,
    criterion_index: usize,
    ch: &Check,
    check_index: usize,
    definition: &DefinitionScope<'_>,
    errs: &mut Vec<ValidationError>,
) {
    let DefinitionScope {
        fixtures,
        inputs,
        pages,
        flows,
        source_map,
        setup_outputs,
        outputs_for,
    } = *definition;
    let check_steps: Vec<(&Step, Option<&str>)> = ch
        .steps
        .iter()
        .flat_map(|s| {
            std::iter::once((s, None)).chain(
                (if s.for_each_body.is_empty() {
                    s.steps.as_deref().unwrap_or(&[])
                } else {
                    &s.for_each_body
                })
                .iter()
                .map(move |body| (body, s.as_binding.as_deref())),
            )
        })
        .collect();
    let source_context_matches =
        source_map.check_context_matches(criterion_index, &c.id, check_index, &ch.id);
    if let Err(error) = ch.worst_case_step_count() {
        errs.push(ValidationError::UncomputableStepCount {
            criterion: c.id.clone(),
            check: ch.id.clone(),
            reason: error.to_string(),
        });
    }
    // A check with neither assertions nor steps can never produce a
    // verdict (the judge would see an empty aggregation). With steps
    // but no assertions the schema layer accepts — whether one of the
    // steps is a judging action (implicit judgment, spec #253) is a
    // catalog question the contract-aware CLI layer answers.
    if ch.assertions.is_empty() && ch.steps.is_empty() {
        errs.push(ValidationError::NothingToJudge {
            criterion: c.id.clone(),
            check: ch.id.clone(),
        });
    }

    let mut step_outputs: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut seen_step_ids: HashSet<&str> = HashSet::new();

    for &(s, _) in &check_steps {
        if let Some(id) = &s.id {
            if !seen_step_ids.insert(id.as_str()) {
                errs.push(ValidationError::DuplicateStepId {
                    criterion: c.id.clone(),
                    check: ch.id.clone(),
                    id: id.clone(),
                });
            }
            let outputs = if let Some(call) = s.call.as_deref() {
                flows
                    .get(call)
                    .map(|flow| flow.outputs.keys().cloned().collect())
                    .unwrap_or_default()
            } else {
                effective_outputs(s, outputs_for)
            };
            step_outputs.insert(id.as_str(), outputs);
        }
    }

    // Declared but not check-body-addressable (#441 Part B) — see
    // `PathScope::out_of_scope_setup_ids`.
    let out_of_scope_setup_ids: HashSet<&str> = c
        .setup
        .iter()
        .chain(ch.setup.iter())
        .filter(|step| {
            !step
                .uses
                .as_deref()
                .is_some_and(|uses| uses.starts_with("process/"))
        })
        .filter_map(|s| s.id.as_deref())
        .collect();

    // #579 exposes only the new process observations from nested setup and
    // consuming fixtures. Existing non-process setup/fixture visibility stays
    // unchanged; resource mutation still has separate ownership validation.
    let mut body_setup_outputs = setup_outputs.clone();
    for step in c
        .setup
        .iter()
        .chain(&ch.setup)
        .flat_map(Step::actions)
        .filter(|step| {
            step.uses
                .as_deref()
                .is_some_and(|uses| uses.starts_with("process/"))
        })
    {
        if let Some(id) = step.id.as_deref() {
            body_setup_outputs.insert(id, effective_outputs(step, outputs_for));
        }
    }
    let mut process_fixture_outputs = HashMap::new();
    for name in &ch.needs {
        if let Some(fixture) = fixtures.get(name) {
            for step in fixture.up.iter().flat_map(Step::actions).filter(|step| {
                step.uses
                    .as_deref()
                    .is_some_and(|uses| uses.starts_with("process/"))
            }) {
                if let Some(id) = step.id.as_deref() {
                    process_fixture_outputs
                        .insert((name.as_str(), id), effective_outputs(step, outputs_for));
                }
            }
        }
    }
    let scope = PathScope {
        process_fixture_outputs: &process_fixture_outputs,
        active_loop: None,
        c,
        ch,
        step_outputs: &step_outputs,
        setup_outputs: &body_setup_outputs,
        out_of_scope_setup_ids: &out_of_scope_setup_ids,
        inputs,
        pages,
    };

    // A browser session is acquired state, not an inline fixture. Only
    // a whole-string path reference is accepted: literals and runtime
    // calls could fabricate auth state at the authoring boundary. Once
    // parsed, the ordinary reference checker supplies the same
    // undeclared input/setup diagnostics as `with:` (#134).
    let mut session_outputs = setup_outputs.clone();
    for step in &c.setup {
        if let Some(id) = step.id.as_deref() {
            session_outputs.insert(id, effective_outputs(step, outputs_for));
        }
    }
    let own_setup_ids = ch.setup.iter().filter_map(|s| s.id.as_deref()).collect();
    let session_scope = PathScope {
        setup_outputs: &session_outputs,
        out_of_scope_setup_ids: &own_setup_ids,
        ..scope
    };
    for (name, raw) in ch
        .session
        .iter()
        .flatten()
        .map(|raw| (None, raw.as_str()))
        .chain(
            ch.sessions
                .iter()
                .flat_map(|sessions| sessions.iter())
                .filter_map(|(name, value)| {
                    value.as_ref().map(|expr| (Some(name), expr.raw.as_str()))
                }),
        )
    {
        let location = source_context_matches
            .then(|| {
                let mut path = crate::source::check_path(
                    criterion_index,
                    check_index,
                    if name.is_some() {
                        "sessions"
                    } else {
                        "session"
                    },
                );
                if let Some(name) = name {
                    path.push(SourcePathSegment::key(name));
                }
                source_map.scalar_location(&path, raw)
            })
            .flatten();
        match crate::expr::parse(raw) {
            Ok(Expr::Path(path)) => {
                if matches!(
                    path.root,
                    PathRoot::Steps | PathRoot::Fixture | PathRoot::Loop
                ) {
                    errs.push(ValidationError::InvalidSessionReference {
                        criterion: c.id.clone(),
                        check: ch.id.clone(),
                        value: raw.to_string(),
                        location,
                    });
                } else {
                    check_path(
                        &session_scope,
                        &path,
                        None,
                        raw,
                        &RefSite::Session,
                        location,
                        errs,
                    );
                }
            }
            _ => errs.push(ValidationError::InvalidSessionReference {
                criterion: c.id.clone(),
                check: ch.id.clone(),
                value: raw.to_string(),
                location,
            }),
        }
    }

    for (assertion_index, assertion) in ch.assertions.iter().enumerate() {
        assertion.walk_exprs(|expr_str| {
            let raw = expr_str.raw.as_str();
            let location = source_context_matches
                .then(|| {
                    source_map.assertion_location(
                        criterion_index,
                        check_index,
                        assertion_index,
                        assertion,
                        raw,
                    )
                })
                .flatten();
            walk_checkable_paths(&expr_str.parsed, &mut |p, arity| {
                check_path(&scope, p, arity, raw, &RefSite::Assertion, location, errs);
            });
        });
    }

    // A condition runs before its action. Loop body outputs are visible
    // only within that iteration, even though assertions can name them.
    let mut preceding = HashMap::new();
    let mut path = crate::source::check_path(criterion_index, check_index, "steps");
    for (index, step) in ch.steps.iter().enumerate() {
        path.push(SourcePathSegment::index(index));
        validate_condition(
            step,
            index,
            &PathScope {
                step_outputs: &preceding,
                ..scope
            },
            source_map,
            source_context_matches.then_some(path.as_slice()),
            errs,
        );
        if step.for_each.is_some() {
            let mut body_outputs = preceding.clone();
            let body = if step.for_each_body.is_empty() {
                step.steps.as_deref().unwrap_or(&[])
            } else {
                &step.for_each_body
            };
            path.push(SourcePathSegment::key("steps"));
            for (inner_index, inner) in body.iter().enumerate() {
                path.push(SourcePathSegment::index(inner_index));
                validate_condition(
                    inner,
                    inner_index,
                    &PathScope {
                        active_loop: step.as_binding.as_deref(),
                        step_outputs: &body_outputs,
                        ..scope
                    },
                    source_map,
                    (source_context_matches && step.for_each_body.is_empty())
                        .then_some(path.as_slice()),
                    errs,
                );
                if let Some(id) = inner.id.as_deref() {
                    body_outputs.insert(id, step_outputs[id].clone());
                }
                path.pop();
            }
            path.pop();
        } else if let Some(id) = step.id.as_deref() {
            preceding.insert(id, step_outputs[id].clone());
        }
        path.pop();
    }

    // Beyond assertions, a `$...` reference inside a step's `with:`
    // payload is the most common place an author writes one — and was
    // historically unscanned, so a typo'd or undeclared ref reached
    // the action as a literal `$...` string (#134). Walk every string
    // scalar in the (untyped) `with:` tree and resolve its references
    // against the same scope.
    for (idx, &(s, active_loop)) in check_steps.iter().enumerate() {
        if let Some(expr) = &s.for_each {
            walk_checkable_paths(&expr.parsed, &mut |p, arity| {
                check_path(
                    &scope,
                    p,
                    arity,
                    &expr.raw,
                    &RefSite::StepWith {
                        step: step_label(s, idx),
                    },
                    None,
                    errs,
                );
            });
        }
        let scope = PathScope {
            active_loop: active_loop.or(s.as_binding.as_deref()),
            ..scope
        };
        let site = RefSite::StepWith {
            step: step_label(s, idx),
        };
        let mut source_path = crate::source::check_path(criterion_index, check_index, "steps");
        source_path.push(SourcePathSegment::index(idx));
        source_path.push(SourcePathSegment::key("with"));
        crate::source::walk_with_strings(&s.with, &mut source_path, &mut |raw, path| {
            if raw.trim_start().starts_with('$')
                && let Err(error) = crate::expr::parse(raw)
            {
                let location = source_context_matches
                    .then(|| {
                        source_map.step_with_location(
                            s,
                            criterion_index,
                            check_index,
                            idx,
                            path,
                            raw,
                        )
                    })
                    .flatten();
                errs.push(ValidationError::InvalidWithExpression {
                    raw: raw.to_string(),
                    context: error.to_string(),
                    site: site.clone(),
                    location,
                });
            }
        });
        crate::source::walk_with_refs(&s.with, &mut source_path, &mut |expr, raw, path| {
            let location = source_context_matches
                .then(|| {
                    source_map.step_with_location(s, criterion_index, check_index, idx, path, raw)
                })
                .flatten();
            walk_checkable_paths(expr, &mut |p, arity| {
                check_path(&scope, p, arity, raw, &site, location, errs);
            });
        });
    }
}

fn validate_condition(
    step: &Step,
    index: usize,
    scope: &PathScope<'_>,
    source_map: &crate::SourceMap,
    step_path: Option<&[SourcePathSegment]>,
    errs: &mut Vec<ValidationError>,
) {
    let StepCondition::Expr(expr) = &step.condition else {
        return;
    };
    let (raw, location) = match &step.condition_origin {
        Some((authored, location)) => (authored.as_str(), Some(*location)),
        None => (
            expr.raw.as_str(),
            step_path.and_then(|path| {
                let mut path = path.to_vec();
                path.push(SourcePathSegment::key("if"));
                source_map.scalar_location(&path, &expr.raw)
            }),
        ),
    };
    let site = RefSite::StepCondition {
        step: step_label(step, index),
    };
    crate::validate_runtime::walk_reference_paths(&expr.parsed, false, &mut |p, arity| {
        if p.root == PathRoot::Runtime {
            let mut errors = Vec::new();
            check_path(scope, p, arity, raw, &site, location, &mut errors);
            errs.extend(
                errors
                    .into_iter()
                    .map(|error| ValidationError::InvalidStepCondition {
                        message: error.to_string(),
                        location,
                    }),
            );
        } else {
            check_path(scope, p, arity, raw, &site, location, errs);
        }
    });
}
