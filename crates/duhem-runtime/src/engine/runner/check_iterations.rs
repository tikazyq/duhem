//! Per-iteration assertion evaluation (#521). Aggregation remains a flat fold.
use super::*;

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_check_iterations(
        &self,
        writer: &mut EvidenceWriter,
        criterion_id: &str,
        authored: &Check,
        ctx: &mut RunContext<'_>,
        contexts: &mut CheckContexts,
        environment_failed: bool,
        browser_missing: bool,
    ) -> Result<(Vec<AssertionOutcome>, Vec<FailedAssertion>, u32), EngineError> {
        // One static index per body action, reused across iterations, just as
        // Tier 1 reuses a template with FlowOrigin.iteration (#514).
        let mut check = authored.clone();
        check.steps = authored
            .steps
            .iter()
            .flat_map(|s| {
                if s.for_each.is_some() {
                    s.for_each_body.clone()
                } else {
                    vec![s.clone()]
                }
            })
            .collect();
        let has_loop = authored.steps.iter().any(|s| s.for_each.is_some());
        let mut evidence = vec![StepEvidence::empty(); check.steps.len()];
        let mut cleanup = BTreeSet::new();
        let mut failed_by = None;
        let mut stored_error = None;
        let mut gated = 0;
        let mut outcomes = Vec::new();
        let mut failed = Vec::new();
        let mut offset = 0;
        let mut ordinal = 0u32;
        let mut ordinary = BTreeSet::new();
        let mut empty_loops = Vec::new();
        for (wrapper_index, step) in authored.steps.iter().enumerate() {
            let control_index = check.assertions.len() + check.steps.len() + wrapper_index;
            let Some(source) = &step.for_each else {
                ordinary.insert(offset);
                self.execute_check_steps(
                    writer,
                    criterion_id,
                    &check,
                    ctx,
                    contexts,
                    offset..offset + 1,
                    None,
                    environment_failed,
                    &mut failed_by,
                    &mut stored_error,
                    &mut cleanup,
                    &mut evidence,
                    &mut gated,
                )
                .await?;
                offset += 1;
                continue;
            };
            let range = offset..offset + step.for_each_body.len();
            offset = range.end;
            // Resolve once in the outer context. Bounds are enforced before
            // dispatch, so excess rows are never silently truncated.
            let gate = match evaluate_gate(step, range.start, failed_by.as_deref(), ctx) {
                Ok(gate) => gate,
                Err(error) => {
                    let detail = writer.mask_text(&error.to_string());
                    let state = VerdictState::Inconclusive(
                        duhem_judge::InconclusiveCause::MissingObservation,
                    );
                    writer
                        .append(EventPayload::StepStarted {
                            criterion_id: criterion_id.to_string(),
                            check_id: check.id.clone(),
                            step_index: control_index as u32,
                            uses: "for_each".into(),
                            layer: None,
                            with: with_to_evidence_map(&step.with),
                            flow: crate::engine::flow::origin(step),
                        })
                        .await?;
                    writer
                        .append(EventPayload::StepFinished {
                            step_index: control_index as u32,
                            outcome: duhem_evidence::StepOutcome::Error,
                            detail: Some(detail.clone()),
                        })
                        .await?;
                    writer
                        .append(EventPayload::AssertionEvaluated {
                            operands: None,
                            check_id: check.id.clone(),
                            assertion_index: control_index as u32,
                            iteration: None,
                            state,
                            detail: Some(detail.clone()),
                            expr: Some("for_each condition resolves".into()),
                            step_index: Some(control_index as u32),
                        })
                        .await?;
                    outcomes.push(AssertionOutcome {
                        assertion_index: control_index,
                        iteration: None,
                        state,
                        detail: Some(detail.clone()),
                    });
                    failed.push(FailedAssertion {
                        expr: "for_each condition resolves".into(),
                        state,
                        detail: Some(detail),
                    });
                    failed_by = Some(display_step_label(step, wrapper_index));
                    continue;
                }
            };
            if let Some(duhem_evidence::StepOutcome::Skipped {
                reason,
                condition,
                operands,
            }) = gate
            {
                for idx in range.clone() {
                    writer
                        .append(EventPayload::StepStarted {
                            criterion_id: criterion_id.to_string(),
                            check_id: check.id.clone(),
                            step_index: idx as u32,
                            uses: check.steps[idx].uses_name().to_string(),
                            layer: duhem_actions::layer_for_uses(check.steps[idx].uses_name())
                                .map(str::to_string),
                            with: with_to_evidence_map(&check.steps[idx].with),
                            flow: crate::engine::flow::origin(&check.steps[idx]),
                        })
                        .await?;
                    writer
                        .append(EventPayload::StepFinished {
                            step_index: idx as u32,
                            outcome: duhem_evidence::StepOutcome::Skipped {
                                reason: reason.clone(),
                                condition: condition.clone(),
                                operands: operands.clone(),
                            },
                            detail: None,
                        })
                        .await?;
                    evidence[idx] = StepEvidence::skipped(reason.clone());
                    evidence[idx].value_gated = condition.is_some();
                    if condition.is_some()
                        && self
                            .registry
                            .get(check.steps[idx].uses_name())
                            .is_some_and(|d| d.judges())
                    {
                        gated += 1;
                    }
                }
                if condition.is_some() {
                    self.judge_iteration(
                        writer,
                        &check,
                        ctx,
                        &evidence,
                        &cleanup,
                        None,
                        range.collect(),
                        true,
                        environment_failed,
                        browser_missing,
                        &mut outcomes,
                        &mut failed,
                    )
                    .await?;
                }
                continue;
            }
            let items = match crate::eval::eval_to_value(&source.parsed, ctx) {
                Ok(Value::Array(items))
                    if items.len() as u64 <= u64::from(step.max.unwrap_or(0)) =>
                {
                    items
                }
                result => {
                    let detail = writer.mask_text(&match result {
                        Ok(Value::Array(items)) => format!(
                            "for_each `{}` produced {} item(s), exceeding max: {}",
                            source.raw,
                            items.len(),
                            step.max.unwrap_or(0)
                        ),
                        Ok(_) => format!("for_each `{}` must evaluate to an array", source.raw),
                        Err(cause) => format!(
                            "for_each `{}` source could not be evaluated: {cause:?}",
                            source.raw
                        ),
                    });
                    writer
                        .append(EventPayload::AssertionEvaluated {
                            operands: None,
                            check_id: check.id.clone(),
                            assertion_index: control_index as u32,
                            iteration: None,
                            state: VerdictState::Fail,
                            detail: Some(detail.clone()),
                            expr: Some("for_each source is an array within max".into()),
                            step_index: None,
                        })
                        .await?;
                    outcomes.push(AssertionOutcome {
                        assertion_index: control_index,
                        iteration: None,
                        state: VerdictState::Fail,
                        detail: Some(detail.clone()),
                    });
                    failed.push(FailedAssertion {
                        expr: "for_each source is an array within max".into(),
                        state: VerdictState::Fail,
                        detail: Some(detail),
                    });
                    failed_by = Some("for_each source".into());
                    continue;
                }
            };
            if items.is_empty() {
                empty_loops.push(control_index);
            }
            let outer = ctx.clone();
            let outer_failed = failed_by.clone();
            let outer_cleanup = cleanup.clone();
            let iteration_count = items.len();
            for (local_ordinal, element) in items.into_iter().enumerate() {
                *ctx = outer.clone();
                if let Some(name) = &step.as_binding {
                    *ctx = ctx.clone().with_loop_binding(name, element);
                }
                failed_by = outer_failed.clone();
                cleanup = outer_cleanup.clone();
                for idx in range.clone() {
                    evidence[idx] = StepEvidence::empty();
                }
                self.execute_check_steps(
                    writer,
                    criterion_id,
                    &check,
                    ctx,
                    contexts,
                    range.clone(),
                    Some(ordinal),
                    environment_failed,
                    &mut failed_by,
                    &mut stored_error,
                    &mut cleanup,
                    &mut evidence,
                    &mut gated,
                )
                .await?;
                let first_failure = failed.len();
                self.judge_iteration(
                    writer,
                    &check,
                    ctx,
                    &evidence,
                    &cleanup,
                    Some(ordinal),
                    range.clone().collect(),
                    true,
                    environment_failed,
                    browser_missing,
                    &mut outcomes,
                    &mut failed,
                )
                .await?;
                // The runtime knows the resolved loop's full cardinality.
                // Keep that presentational context in the existing failure
                // detail, shared by default and plugin reporters (#519).
                // The authored step identifies the loop when a check has
                // several; ordinals are one-based within that loop.
                for failure in &mut failed[first_failure..] {
                    let location = format!(
                        "for_each step {}, iteration {} of {}",
                        wrapper_index + 1,
                        local_ordinal + 1,
                        iteration_count,
                    );
                    failure.detail = Some(match failure.detail.take() {
                        Some(detail) => format!("{location}: {detail}"),
                        None => location,
                    });
                }
                ordinal += 1;
            }
            *ctx = outer;
            failed_by = outer_failed;
            cleanup = outer_cleanup;
        }
        writer.set_session(None);
        if let Some(error) = stored_error {
            return Err(error);
        }
        // Ordinary checks retain their single evaluation. In a loop check,
        // explicit assertions have already been evaluated N times (zero for
        // an empty source), while ordinary actions contribute implicit verdicts.
        self.judge_iteration(
            writer,
            &check,
            ctx,
            &evidence,
            &cleanup,
            None,
            ordinary,
            !has_loop,
            environment_failed,
            browser_missing,
            &mut outcomes,
            &mut failed,
        )
        .await?;
        // A zero-row obligation cannot disappear behind another loop's or
        // ordinary action's passing judgments. The judge still uses its
        // unchanged flat fold. A wholly empty check needs no synthetic slot.
        if !outcomes.is_empty() {
            for index in empty_loops {
                let state =
                    VerdictState::Inconclusive(duhem_judge::InconclusiveCause::EmptyAggregation);
                writer
                    .append(EventPayload::AssertionEvaluated {
                        operands: None,
                        check_id: check.id.clone(),
                        assertion_index: index as u32,
                        iteration: None,
                        state,
                        detail: Some("for_each source is empty".into()),
                        expr: None,
                        step_index: None,
                    })
                    .await?;
                outcomes.push(AssertionOutcome {
                    assertion_index: index,
                    iteration: None,
                    state,
                    detail: Some("for_each source is empty".into()),
                });
            }
        }
        Ok((outcomes, failed, gated))
    }

    #[allow(clippy::too_many_arguments)]
    async fn judge_iteration(
        &self,
        writer: &mut EvidenceWriter,
        check: &Check,
        ctx: &RunContext<'_>,
        evidence: &[StepEvidence],
        cleanup: &BTreeSet<usize>,
        iteration: Option<u32>,
        indices: BTreeSet<usize>,
        explicit: bool,
        environment_failed: bool,
        browser_missing: bool,
        outcomes: &mut Vec<AssertionOutcome>,
        failed: &mut Vec<FailedAssertion>,
    ) -> Result<(), EngineError> {
        writer.set_session(None);
        if explicit {
            let any_unknown = indices
                .iter()
                .any(|&i| !self.registry.contains_key(check.steps[i].uses_name()));
            evaluate_explicit_assertions(
                writer,
                check,
                ctx,
                any_unknown,
                iteration,
                environment_failed,
                browser_missing,
                evidence,
                cleanup,
                outcomes,
                failed,
            )
            .await?;
        }
        let mut excluded = cleanup.clone();
        excluded.extend((0..check.steps.len()).filter(|i| !indices.contains(i)));
        let implicit = implicit_judgment_outcomes(
            check,
            |uses| self.registry.get(uses).is_some_and(|d| d.judges()),
            |uses| self.registry.contains_key(uses),
            evidence,
            environment_failed,
            browser_missing,
            &excluded,
        );
        append_implicit_judgment(
            writer,
            &check.id,
            implicit,
            check.assertions.len(),
            iteration,
            outcomes,
            failed,
        )
        .await
    }
}
