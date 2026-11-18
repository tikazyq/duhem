use duhem_evidence::{EventPayload, ReplayDivergence, ReplayError, Trace, replay, replay_aborted};

fn trace(payloads: &[serde_json::Value]) -> Trace {
    let events = payloads
        .iter()
        .enumerate()
        .map(|(seq, payload)| {
            let mut value = payload.clone();
            value["seq"] = serde_json::json!(seq);
            value["ts"] = serde_json::json!("2026-11-18T00:00:00.000Z");
            serde_json::from_value(value).unwrap()
        })
        .collect();
    Trace::from_events(events).unwrap()
}

fn partial() -> Vec<serde_json::Value> {
    use serde_json::json;
    vec![
        json!({"kind":"run_started", "verification_path":"x.yml", "schema_version":"v1"}),
        json!({"kind":"assertion_evaluated", "check_id":"complete", "assertion_index":0, "state":"pass"}),
        json!({"kind":"check_finished", "check_id":"complete", "criterion_id":"done", "verdict":"pass"}),
        json!({"kind":"criterion_finished", "criterion_id":"done", "verdict":"pass"}),
        json!({"kind":"assertion_evaluated", "check_id":"checked", "assertion_index":0, "state":"pass"}),
        json!({"kind":"check_finished", "check_id":"checked", "criterion_id":"open", "verdict":"pass"}),
        json!({"kind":"step_started", "criterion_id":"open", "check_id":"unfinished", "step_index":0, "uses":"cli/invoke"}),
        json!({"kind":"assertion_evaluated", "check_id":"unfinished", "assertion_index":0, "state":"fail"}),
        json!({"kind":"run_aborted", "signal":"engine_error", "error":"unresolved reference"}),
    ]
}

#[test]
fn aborted_replay_allows_unfinished_scopes_but_strict_replay_requires_judgment() {
    let trace = trace(&partial());
    replay_aborted(&trace).unwrap();
    assert_eq!(replay(&trace), Err(ReplayError::MissingRunFinished));
}

#[test]
fn aborted_replay_checks_every_completed_judgment() {
    for index in [1, 2, 4, 5] {
        let mut events = partial();
        let field = if index == 1 || index == 4 {
            "state"
        } else {
            "verdict"
        };
        events[index][field] = serde_json::json!("fail");
        assert!(matches!(
            replay_aborted(&trace(&events)),
            Err(ReplayError::Divergence(ReplayDivergence::Check { .. }))
        ));
    }
    let mut events = partial();
    events[3]["verdict"] = serde_json::json!("fail");
    assert!(matches!(
        replay_aborted(&trace(&events)),
        Err(ReplayError::Divergence(ReplayDivergence::Criterion { .. }))
    ));
}

#[test]
fn aborted_replay_requires_a_single_last_abort() {
    let events = partial();
    assert_eq!(
        replay_aborted(&trace(&events[..8])),
        Err(ReplayError::MissingRunAborted)
    );
    let mut extra = events.clone();
    extra.push(serde_json::json!({"kind":"run_heartbeat"}));
    assert!(matches!(
        replay_aborted(&trace(&extra)),
        Err(ReplayError::OutOfOrder(_))
    ));
    let mut extra = events;
    extra.push(serde_json::json!({"kind":"run_aborted", "signal":"SIGTERM"}));
    assert!(matches!(
        replay_aborted(&trace(&extra)),
        Err(ReplayError::OutOfOrder(_))
    ));
}

#[test]
fn aborted_replay_rejects_overwritten_judgments_and_unfinished_completed_criteria() {
    for (index, payload) in [
        (
            3,
            serde_json::json!({"kind":"check_finished", "check_id":"complete", "criterion_id":"done", "verdict":"fail"}),
        ),
        (
            4,
            serde_json::json!({"kind":"criterion_finished", "criterion_id":"done", "verdict":"fail"}),
        ),
        (
            3,
            serde_json::json!({"kind":"assertion_evaluated", "check_id":"complete", "assertion_index":1, "state":"fail"}),
        ),
        (
            8,
            serde_json::json!({"kind":"criterion_finished", "criterion_id":"open", "verdict":"pass"}),
        ),
    ] {
        let mut events = partial();
        events.insert(index, payload);
        assert!(matches!(
            replay_aborted(&trace(&events)),
            Err(ReplayError::OutOfOrder(_))
        ));
    }
}

#[test]
fn aborted_replay_preserves_policy_and_diagnostic_exemption() {
    let mut events = partial();
    events[0]["inconclusive_policy"] = serde_json::json!("warn");
    events[1]["state"] = serde_json::json!("inconclusive:missing_observation");
    events[2]["verdict"] = serde_json::json!("inconclusive:missing_observation");
    events.insert(2, serde_json::json!({"kind":"assertion_evaluated", "check_id":"complete", "assertion_index":1, "state":"fail", "diagnostic":true}));
    replay_aborted(&trace(&events)).unwrap();
}

#[test]
fn operator_abort_and_abort_before_any_judgment_replay() {
    let events = partial();
    for terminal in [
        serde_json::json!({"kind":"run_aborted", "signal":"SIGTERM"}),
        events[8].clone(),
    ] {
        let trace = trace(&[events[0].clone(), terminal]);
        replay_aborted(&trace).unwrap();
        assert!(matches!(
            trace.events()[1].payload,
            EventPayload::RunAborted { .. }
        ));
    }
}
