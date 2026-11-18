//! Value conditions exercise real SQLite actions and the persisted trace.
use std::{collections::BTreeMap, sync::Arc};

use duhem_evidence::{EventPayload, SqliteStore, StepOutcome, Trace};
use duhem_judge::{InconclusiveCause, VerdictState};
use duhem_runtime::Engine;

async fn run(source: &str) -> (VerdictState, Trace) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("duhem.yml");
    std::fs::write(&path, source).unwrap();
    let loaded = duhem_schema::load(&path).unwrap();
    let duhem_schema::Loaded::Leaf { definition, .. } = loaded else {
        panic!("expected leaf")
    };
    duhem_schema::validate_with_contract_outputs(&definition, &|uses| {
        duhem_actions::catalog()
            .into_iter()
            .find(|c| c.uses == uses)
            .map(|c| c.outputs.into_iter().map(str::to_string).collect())
            .unwrap_or_default()
    })
    .unwrap();
    let store = Arc::new(
        SqliteStore::open(tmp.path().join("evidence.db"))
            .await
            .unwrap(),
    );
    let outcome = Engine::new()
        .with_store(store.clone())
        .run_with_metadata(&definition, BTreeMap::new())
        .await
        .unwrap();
    let trace = Trace::from_store(store.as_ref(), &outcome.run_id)
        .await
        .unwrap();
    (outcome.verdict.state, trace)
}

fn definition(steps: &str, assertions: &str) -> String {
    format!(
        r#"
verification: check-conditions
criteria:
  - id: AC-1
    description: Conditional observations retain honest judgments
    checks:
      - id: C
        steps:
          - id: first
            uses: db/query
            with: {{connection: 'sqlite::memory:', sql: 'select 1 as value'}}
{steps}
        assertions: [{assertions}]
      - id: later_check
        assertions: ["true"]
"#
    )
}

#[tokio::test]
async fn true_and_false_conditions_use_earlier_observations() {
    let (state, trace) = run(&definition(
        r#"          - id: yes
            uses: db/query
            if: $steps.first.outputs.row_count == 1
            with: {connection: 'sqlite::memory:', sql: 'select 2'}
          - id: no
            uses: db/observe
            if: $steps.first.outputs.row_count == 0
            with: {connection: 'sqlite::memory:', sql: 'select 3', until: {row_count: 1}}"#,
        "'$steps.yes.outputs.row_count == 1'",
    ))
    .await;
    assert_eq!(state, VerdictState::Pass);
    assert!(trace.events().iter().any(|e| matches!(
        &e.payload,
        EventPayload::StepFinished {
            step_index: 2,
            outcome: StepOutcome::Skipped {
                condition: Some(_),
                ..
            },
            ..
        }
    )));
    assert!(trace.events().iter().any(|e| matches!(&e.payload,
        EventPayload::CheckFinished {check_id, gated_judging_steps: 1, ..} if check_id == "C")));
}

#[tokio::test]
async fn gated_output_assertion_is_missing_instead_of_omitted() {
    let (state, trace) = run(&definition(
        r#"          - id: gated
            uses: db/query
            if: $steps.first.outputs.row_count == 0
            with: {connection: 'sqlite::memory:', sql: 'select 2'}"#,
        "'true', '$steps.gated.outputs.row_count == 1', {exists: '$steps.gated.outputs.row_count'}",
    ))
    .await;
    let missing = VerdictState::Inconclusive(InconclusiveCause::MissingObservation);
    assert_eq!(state, missing);
    assert!(trace.events().iter().any(|e| matches!(&e.payload,
        EventPayload::AssertionEvaluated {check_id, assertion_index: 1, state, ..}
            if check_id == "C" && *state == missing)));
}

#[tokio::test]
async fn unresolved_conditions_error_the_step_and_continue_the_run() {
    for body in [
        r#"          - id: undecidable
            uses: db/query
            if: $steps.first.outputs.rows[0].missing == 1
            with: {connection: 'sqlite::memory:', sql: 'select 2'}"#,
        r#"          - for_each: $steps.first.outputs.rows
            max: 2
            if: $steps.first.outputs.rows[0].missing == 1
            uses: db/query
            with: {connection: 'sqlite::memory:', sql: 'select 2'}"#,
    ] {
        let (state, trace) = run(&definition(body, "'true'")).await;
        assert_eq!(
            state,
            VerdictState::Inconclusive(InconclusiveCause::MissingObservation)
        );
        assert!(trace.events().iter().any(|e| matches!(&e.payload,
            EventPayload::StepFinished {outcome: StepOutcome::Error, detail: Some(detail), ..}
                if detail.contains("missing") && detail.contains("step"))));
        assert!(trace.events().iter().any(|e| matches!(&e.payload,
            EventPayload::CheckFinished {check_id, ..} if check_id == "later_check")));
    }
}

#[tokio::test]
async fn false_wrapper_condition_keeps_output_assertions_inconclusive() {
    let (state, trace) = run(&definition(
        r#"          - for_each: $steps.first.outputs.rows
            max: 2
            if: $steps.first.outputs.row_count == 0
            steps:
              - id: gated
                uses: db/observe
                with: {connection: 'sqlite::memory:', sql: 'select 2', until: {row_count: 1}}"#,
        "'true', '$steps.gated.outputs.row_count == 1'",
    ))
    .await;
    assert_eq!(
        state,
        VerdictState::Inconclusive(InconclusiveCause::MissingObservation)
    );
    assert!(trace.events().iter().any(|e| matches!(&e.payload,
        EventPayload::CheckFinished {check_id, gated_judging_steps: 1, ..} if check_id == "C")));
}

#[tokio::test]
async fn flow_call_conditions_apply_to_every_expanded_step_and_rewrite_outputs() {
    let source = format!(
        r#"
flows:
  query:
    steps:
      - id: result
        uses: db/query
        with: {{connection: 'sqlite::memory:', sql: 'select 1'}}
      - uses: db/query
        with: {{connection: 'sqlite::memory:', sql: 'select 2'}}
    outputs:
      count: $steps.result.outputs.row_count
{}"#,
        definition(
            r#"          - id: producer
            call: query
          - id: gated
            call: query
            if: $steps.producer.outputs.count == 0
          - id: accepted
            call: query
            if: $steps.producer.outputs.count == 1"#,
            "'$steps.accepted.outputs.count == 1'"
        )
    );
    let (state, trace) = run(&source).await;
    assert_eq!(state, VerdictState::Pass);
    assert_eq!(
        trace
            .events()
            .iter()
            .filter(|e| matches!(
                &e.payload,
                EventPayload::StepFinished {
                    outcome: StepOutcome::Skipped {
                        condition: Some(_),
                        ..
                    },
                    ..
                }
            ))
            .count(),
        2
    );
}
