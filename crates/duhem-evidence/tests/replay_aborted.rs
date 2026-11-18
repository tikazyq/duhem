use duhem_evidence::{
    Event, EventPayload, ReplayDivergence, ReplayError, Trace, VerdictState, replay, replay_aborted,
};
use serde_json::json;

fn abort_events() -> Vec<Event> {
    [
        json!({"kind": "run_started", "verification_path": "case.yml", "inputs": {}, "schema_version": duhem_evidence::SCHEMA_VERSION}),
        json!({"kind": "assertion_evaluated", "check_id": "done", "assertion_index": 0, "state": "pass"}),
        json!({"kind": "check_finished", "check_id": "done", "criterion_id": "AC-1", "verdict": "pass"}),
        json!({"kind": "criterion_finished", "criterion_id": "AC-1", "verdict": "pass"}),
        json!({"kind": "assertion_evaluated", "check_id": "sibling", "assertion_index": 0, "state": "pass"}),
        json!({"kind": "check_finished", "check_id": "sibling", "criterion_id": "AC-2", "verdict": "pass"}),
        json!({"kind": "step_started", "criterion_id": "AC-2", "check_id": "interrupted", "step_index": 0, "uses": "cli/invoke"}),
        json!({"kind": "assertion_evaluated", "check_id": "interrupted", "assertion_index": 0, "state": "fail"}),
        // Legacy operator aborts omit `error` and may also have partial judgment evidence.
        json!({"kind": "run_aborted", "signal": "SIGTERM"}),
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, mut event)| {
        event["seq"] = json!(seq);
        event["ts"] = json!("2026-09-18T00:00:00.000Z");
        serde_json::from_value(event).unwrap()
    })
    .collect()
}

#[test]
fn replay_accepts_open_scopes_and_legacy_operator_abort() {
    let trace = Trace::from_events(abort_events()).unwrap();
    let terminal = trace.events().last().unwrap();
    assert!(matches!(
        &terminal.payload,
        EventPayload::RunAborted { signal, error: None } if signal == "SIGTERM"
    ));
    assert!(
        serde_json::to_value(terminal)
            .unwrap()
            .get("error")
            .is_none()
    );
    assert_eq!(replay_aborted(&trace), Ok(()));
    assert_eq!(replay(&trace), Err(ReplayError::MissingRunFinished));
}

#[test]
fn replay_rejects_divergent_completed_checks_even_in_an_open_criterion() {
    let mut events = abort_events();
    let EventPayload::CheckFinished { verdict, .. } = &mut events[5].payload else {
        panic!("expected a completed check");
    };
    *verdict = VerdictState::Fail;
    let trace = Trace::from_events(events).unwrap();
    assert!(matches!(
        replay_aborted(&trace),
        Err(ReplayError::Divergence(ReplayDivergence::Check { check_id, .. })) if check_id == "sibling"
    ));
}

#[test]
fn replay_rejects_divergent_completed_criteria() {
    let mut events = abort_events();
    let EventPayload::CriterionFinished { verdict, .. } = &mut events[3].payload else {
        panic!("expected a completed criterion");
    };
    *verdict = VerdictState::Fail;
    let trace = Trace::from_events(events).unwrap();
    assert!(matches!(
        replay_aborted(&trace),
        Err(ReplayError::Divergence(ReplayDivergence::Criterion { criterion_id, .. })) if criterion_id == "AC-1"
    ));
}

#[test]
fn replay_requires_one_abort_at_the_end() {
    let mut events = abort_events();
    let mut terminal = events.pop().unwrap();
    let trace = Trace::from_events(events.clone()).unwrap();
    assert_eq!(replay_aborted(&trace), Err(ReplayError::MissingRunAborted));
    events.push(terminal.clone());
    terminal.seq += 1;
    events.push(terminal);
    let trace = Trace::from_events(events).unwrap();
    assert!(matches!(
        replay_aborted(&trace),
        Err(ReplayError::OutOfOrder(_))
    ));
}
