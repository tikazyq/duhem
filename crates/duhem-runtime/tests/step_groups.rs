//! Inline groups through the loader, SQLite actions and persisted evidence.
use std::sync::Arc;

use duhem_evidence::{EventPayload, SqliteStore, StepOutcome, Trace, replay};
use duhem_judge::VerdictState;
use duhem_runtime::Engine;

async fn run(yaml: &str) -> (VerdictState, u32, Trace) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("verification.yml");
    std::fs::write(&path, yaml).unwrap();
    let duhem_schema::Loaded::Leaf { definition, .. } = duhem_schema::load(&path).unwrap() else {
        panic!("expected leaf");
    };
    duhem_schema::validate_with_action_catalog(&definition, &|uses| {
        duhem_actions::contract_for(uses).map(|c| c.outputs.iter().map(|o| o.to_string()).collect())
    })
    .unwrap();
    let store = Arc::new(
        SqliteStore::open(dir.path().join("evidence.db"))
            .await
            .unwrap(),
    );
    let inputs = definition
        .inputs
        .iter()
        .filter_map(|(name, decl)| {
            decl.default
                .as_ref()
                .map(|value| (name.clone(), serde_json::to_value(value).unwrap()))
        })
        .collect();
    let outcome = Engine::new()
        .with_store(store.clone())
        .run_with_metadata(&definition, inputs)
        .await
        .unwrap();
    let trace = Trace::from_store(store.as_ref(), &outcome.run_id)
        .await
        .unwrap();
    assert_eq!(replay(&trace).unwrap().run.state, outcome.verdict.state);
    (
        outcome.verdict.state,
        outcome.gated_checks.values().sum(),
        trace,
    )
}

#[tokio::test]
async fn groups_share_gate_state_and_keep_flat_indices() {
    let (state, gated, trace) = run(r#"
verification: grouped checks
criteria:
  - id: AC-1
    description: Grouped diagnostics run after failure
    checks:
      - id: AC-1.1
        steps:
          - id: operation
            steps:
              - id: fail
                uses: db/observe
                with: { connection: 'sqlite::memory:', sql: 'select 1 where 0', until: { row_count: 1 }, timeout: 30ms }
              - uses: db/query
                with: &query { connection: 'sqlite::memory:', sql: 'select 1' }
          - id: blocked
            steps:
              - id: nested
                if: always
                steps:
                  - uses: db/query
                    with: *query
          - id: diagnostics
            if: failure
            steps:
              - uses: db/query
                with: *query
              - uses: db/query
                if: success
                with: *query
          - id: cleanup
            if: always
            steps:
              - id: inner
                steps:
                  - uses: db/query
                    with: *query
          - uses: db/query
            with: *query
"#).await;
    assert_eq!(state, VerdictState::Fail);
    assert_eq!(gated, 0);
    let started: Vec<_> = trace
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::StepStarted {
                step_index,
                group_path,
                ..
            } => Some((*step_index, group_path.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![
            (0, Some(vec!["operation".into()])),
            (1, Some(vec!["operation".into()])),
            (2, Some(vec!["blocked".into(), "nested".into()])),
            (3, Some(vec!["diagnostics".into()])),
            (4, Some(vec!["diagnostics".into()])),
            (5, Some(vec!["cleanup".into(), "inner".into()])),
            (6, None),
        ]
    );
    for event in trace.events() {
        if let EventPayload::StepFinished {
            step_index,
            outcome,
            ..
        } = &event.payload
        {
            match step_index {
                1 | 4 | 6 => assert!(
                    matches!(outcome, StepOutcome::Skipped { reason, .. } if reason.contains("fail"))
                ),
                2 => assert!(
                    matches!(outcome, StepOutcome::Skipped { reason, .. } if reason.contains("group `blocked`"))
                ),
                3 | 5 => assert_eq!(*outcome, StepOutcome::Ok),
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn false_group_conditions_are_reported_in_checks_and_lifecycle() {
    let (state, gated, trace) = run(r#"
verification: value gated group
setup:
  - id: probe
    uses: db/query
    with: { connection: 'sqlite::memory:', sql: 'select 1 where 0' }
  - id: prepare
    if: $setup.probe.outputs.row_count > 0
    steps:
      - uses: db/query
        with: { connection: 'sqlite::memory:', sql: 'select 1' }
criteria:
  - id: AC-1
    description: Conditional checks report gated claims
    checks:
      - id: AC-1.1
        steps:
          - id: optional
            if: $setup.probe.outputs.row_count > 0
            steps:
              - uses: db/observe
                with: { connection: 'sqlite::memory:', sql: 'select 1', until: { row_count: 1 } }
              - uses: db/observe
                if: always
                with: { connection: 'sqlite::memory:', sql: 'select 1', until: { row_count: 1 } }
        assertions: ["true"]
"#)
    .await;
    assert_eq!(state, VerdictState::Pass);
    assert_eq!(gated, 2);
    for (label, lifecycle, count) in [("prepare", true, 1), ("optional", false, 2)] {
        let events = trace.events().iter().filter(|e| {
            let outcome = match &e.payload {
                EventPayload::SetupStepFinished { outcome, .. } if lifecycle => outcome,
                EventPayload::StepFinished { outcome, .. } if !lifecycle => outcome,
                _ => return false,
            };
            matches!(outcome, StepOutcome::Skipped { reason, condition: Some(condition), operands: Some(operands) }
                if reason.contains(label) && condition == "$setup.probe.outputs.row_count > 0"
                    && operands.get("$setup.probe.outputs.row_count") == Some(&serde_json::json!(0)))
        }).count();
        assert_eq!(events, count);
    }
    assert!(trace.events().iter().any(|event| matches!(&event.payload,
        EventPayload::SetupStepStarted { step_index: 1, group_path: Some(path), .. }
            if path == &["prepare"]
    )));
}

#[tokio::test]
async fn loop_groups_recheck_the_condition_for_each_iteration() {
    let (state, _, trace) = run(r#"
verification: grouped iteration
inputs:
  rows: { type: array, default: [0, 1] }
criteria:
  - id: AC-1
    description: Positive rows are judged
    checks:
      - id: AC-1.1
        steps:
          - for_each: $inputs.rows
            max: 2
            as: row
            steps:
              - id: positive
                if: $row > 0
                steps:
                  - uses: db/observe
                    with: { connection: 'sqlite::memory:', sql: 'select 1', until: { row_count: 1 } }
        assertions: ["true"]
"#).await;
    assert_eq!(state, VerdictState::Pass);
    let iterations: Vec<_> = trace
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::StepStarted {
                step_index: 0,
                group_path: Some(path),
                flow: Some(flow),
                ..
            } if path.as_slice() == ["positive"] => flow.iteration,
            _ => None,
        })
        .collect();
    assert_eq!(iterations, [0, 1]);
    let outcomes: Vec<_> = trace
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::StepFinished { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    assert!(
        matches!(outcomes.as_slice(), [StepOutcome::Skipped { reason, .. }, StepOutcome::Ok]
        if reason.contains("group `positive`"))
    );
}
