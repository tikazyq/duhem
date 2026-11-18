//! Closed step-gating policy shared by checks and setup.

use duhem_actions::Outcome;
use duhem_evidence::StepOutcome;
use duhem_judge::VerdictState;
use duhem_schema::{Step, StepCondition, StepGroup};

pub(crate) fn skip_reason(condition: &StepCondition, failed_by: Option<&str>) -> Option<String> {
    match (condition, failed_by) {
        // A value expression carries `success` semantics for the outcome
        // gate: omitting `if:` means `success`, so replacing it with an
        // expression must not silently forfeit that protection. The
        // expression is evaluated only once this gate passes.
        (
            StepCondition::Success | StepCondition::ExplicitSuccess | StepCondition::Expr(_),
            Some(step),
        ) => Some(format!("blocked by failed step `{step}`")),
        (StepCondition::Failure, None) => {
            Some("`if: failure` requires an earlier failed step".to_string())
        }
        (
            StepCondition::Success
            | StepCondition::ExplicitSuccess
            | StepCondition::Always
            | StepCondition::Expr(_),
            None,
        )
        | (StepCondition::Always | StepCondition::Failure, Some(_)) => None,
    }
}

pub(crate) fn step_failed(outcome: &Outcome, judgment: Option<VerdictState>) -> bool {
    matches!(outcome, Outcome::Error | Outcome::Timeout)
        || matches!(judgment, Some(VerdictState::Fail))
}

/// Apply the outcome gate first, then observe the value condition once.
/// Lifecycle and `for_each` steps treat an unresolvable condition as an
/// engine error (the step fails and its dependents go inconclusive).
pub(super) fn evaluate(
    step: &duhem_schema::Step,
    index: usize,
    failed_by: Option<&str>,
    ctx: &dyn crate::EvalContext,
) -> Result<Option<duhem_evidence::StepOutcome>, super::EngineError> {
    gate(&step.condition, failed_by, ctx).map_err(|cause| super::EngineError::UnresolvedReference {
        reference: condition_source(step),
        context: format!(" (condition could not be evaluated: {cause:?})"),
        step: super::outcome::step_label(step, index),
    })
}

/// Check-step counterpart of [`evaluate`] (#440 R2): an unresolvable
/// condition is not a run abort. The caller records the returned detail
/// as an errored step, which the check's implicit judgment turns into
/// `Inconclusive(MissingObservation)`.
pub(super) fn evaluate_in_check(
    step: &duhem_schema::Step,
    index: usize,
    failed_by: Option<&str>,
    ctx: &dyn crate::EvalContext,
) -> Result<Option<duhem_evidence::StepOutcome>, String> {
    gate(&step.condition, failed_by, ctx).map_err(|cause| {
        format!(
            "step `{}`: condition `{}` could not be resolved: {}",
            super::outcome::step_label(step, index),
            condition_source(step),
            super::translate::eval_cause_detail(&cause)
        )
    })
}

fn condition_source(step: &duhem_schema::Step) -> String {
    match &step.condition {
        StepCondition::Expr(expr) => expr.raw.clone(),
        _ => String::new(),
    }
}

fn gate(
    condition: &StepCondition,
    failed_by: Option<&str>,
    ctx: &dyn crate::EvalContext,
) -> Result<Option<duhem_evidence::StepOutcome>, crate::InconclusiveCause> {
    if let Some(reason) = skip_reason(condition, failed_by) {
        return Ok(Some(StepOutcome::Skipped {
            reason,
            condition: None,
            operands: None,
        }));
    }
    let StepCondition::Expr(expr) = condition else {
        return Ok(None);
    };
    let (result, operands) = crate::eval::operands::evaluate(expr, ctx);
    match result {
        crate::EvalResult::True => Ok(None),
        crate::EvalResult::False => Ok(Some(StepOutcome::Skipped {
            reason: format!("condition `{}` evaluated false", expr.raw),
            condition: Some(expr.raw.clone()),
            operands: Some(operands),
        })),
        crate::EvalResult::Inconclusive(cause) => Err(cause),
    }
}

/// Conditions are evaluated when entering a group, then reused until the
/// sequence leaves it. Each loop iteration starts with its own cursor.
#[derive(Clone, Default)]
pub(super) struct GroupGates {
    active: Vec<(StepGroup, Option<StepOutcome>)>,
}

impl GroupGates {
    pub(super) fn enter(
        &mut self,
        step: &Step,
        failed_by: Option<&str>,
        ctx: &dyn crate::EvalContext,
    ) -> Result<Option<StepOutcome>, super::EngineError> {
        let common = self
            .active
            .iter()
            .zip(&step.groups)
            .take_while(|((active, _), group)| active == *group)
            .count();
        self.active.truncate(common);
        for group in &step.groups[common..] {
            let outcome = if let Some((_, Some(blocked))) = self.active.last() {
                Some(blocked.clone())
            } else {
                match gate(&group.condition, failed_by, ctx) {
                    Ok(Some(StepOutcome::Skipped {
                        reason,
                        condition,
                        operands,
                    })) => Some(StepOutcome::Skipped {
                        reason: if group.label.is_empty() {
                            reason
                        } else {
                            format!("group `{}`: {reason}", group.label)
                        },
                        condition,
                        operands,
                    }),
                    Ok(outcome) => outcome,
                    Err(cause) => {
                        let step = format!("group `{}`", group.label);
                        self.active.push((
                            group.clone(),
                            Some(StepOutcome::Skipped {
                                reason: format!("{step}: condition could not be evaluated"),
                                condition: None,
                                operands: None,
                            }),
                        ));
                        let StepCondition::Expr(expr) = &group.condition else {
                            unreachable!()
                        };
                        return Err(super::EngineError::UnresolvedReference {
                            reference: expr.raw.clone(),
                            context: format!(" (condition could not be evaluated: {cause:?})"),
                            step,
                        });
                    }
                }
            };
            self.active.push((group.clone(), outcome));
        }
        Ok(self.active.last().and_then(|(_, outcome)| outcome.clone()))
    }
}

pub(super) fn group_path(step: &Step) -> Option<Vec<String>> {
    let labels: Vec<_> = step
        .groups
        .iter()
        .filter(|group| !group.label.is_empty())
        .map(|group| group.label.clone())
        .collect();
    (!labels.is_empty()).then_some(labels)
}
