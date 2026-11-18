//! The real SQLite actions, loader, and judge exercise value-based check gates.
use std::sync::Arc;

use duhem_evidence::{EventPayload, SqliteStore, StepOutcome, Trace, replay};
use duhem_judge::{InconclusiveCause, VerdictState};
use duhem_runtime::Engine;

const EXAMPLE: &str = include_str!("../../../verifications/check-conditions-example/duhem.yml");
const MISSING: VerdictState = VerdictState::Inconclusive(InconclusiveCause::MissingObservation);

async fn run(yaml: &str) -> (VerdictState, Trace) {
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
    let result = Engine::new()
        .with_store(store.clone())
        .run_with_metadata(&definition, Default::default())
        .await
        .unwrap();
    let trace = Trace::from_store(store.as_ref(), &result.run_id)
        .await
        .unwrap();
    assert_eq!(replay(&trace).unwrap().run.state, result.verdict.state);
    (result.verdict.state, trace)
}

#[tokio::test]
async fn a_call_condition_gates_each_expanded_step_and_reads_projected_outputs() {
    let (state, trace) = run(EXAMPLE).await;
    assert_eq!(state, VerdictState::Pass);
    assert_eq!(trace.events().iter().filter(|event| matches!(&event.payload,
        EventPayload::StepFinished { outcome: StepOutcome::Skipped { condition: Some(condition), operands: Some(operands), .. }, .. }
            if condition == "$steps.jobs.outputs.count == 0" && operands.values().any(|value| value == &serde_json::json!(1))
    )).count(), 2);
    assert!(trace.events().iter().any(|event| matches!(
        event.payload,
        EventPayload::CheckFinished {
            gated_judging_steps: 2,
            ..
        }
    )));
}

#[tokio::test]
async fn assertions_on_gated_outputs_are_missing_including_exists() {
    let yaml = format!(
        "{EXAMPLE}          - exists: $steps.when_empty.outputs.satisfied\n          - $steps.when_empty.outputs.satisfied == true\n"
    );
    let (state, trace) = run(&yaml).await;
    assert_eq!(state, MISSING);
    for index in [1, 2] {
        assert!(trace.events().iter().any(|event| matches!(&event.payload,
            EventPayload::AssertionEvaluated { assertion_index, state, detail: Some(detail), .. }
                if *assertion_index == index && *state == MISSING && detail.contains("when_empty")
        )));
    }
}

const ERROR_EXAMPLE: &str = r#"
verification: Missing condition operand
criteria:
  - id: AC-1
    description: An undecidable condition leaves evidence and does not abort the run.
    checks:
      - id: C1
        steps:
          - id: probe
            uses: db/query
            with: { connection: 'sqlite::memory:', sql: 'select 1 as present' }
          - id: gated
            uses: db/query
            if: $steps.probe.outputs.rows[0].missing == true
            with: { connection: 'sqlite::memory:', sql: 'select 1' }
        assertions: ["true"]
      - id: C2
        assertions: ["true"]
"#;

#[tokio::test]
async fn unresolved_conditions_record_step_errors_and_allow_sibling_checks() {
    let wrapper = ERROR_EXAMPLE.replace(
        "          - id: gated\n            uses:",
        "          - for_each: $steps.probe.outputs.rows\n            max: 2\n            uses:",
    );
    for (yaml, label) in [(ERROR_EXAMPLE, "gated"), (&wrapper, "for_each #2")] {
        let (state, trace) = run(yaml).await;
        assert_eq!(state, MISSING);
        assert!(trace.events().iter().any(|event| matches!(&event.payload,
            EventPayload::StepFinished { outcome: StepOutcome::Error, detail: Some(detail), .. }
                if detail.contains(label) && detail.contains("$steps.probe.outputs.rows[0].missing")
        )));
        assert!(trace.events().iter().any(|event| matches!(&event.payload,
            EventPayload::CheckFinished { check_id, verdict: VerdictState::Pass, .. }
                if check_id == "C2"
        )));
    }
}

#[tokio::test]
async fn wrapper_false_gates_the_body_and_assertions_see_missing_outputs() {
    let yaml = ERROR_EXAMPLE
        .replace("          - id: gated\n            uses: db/query\n            if: $steps.probe.outputs.rows[0].missing == true\n            with: { connection: 'sqlite::memory:', sql: 'select 1' }", r#"          - for_each: $steps.probe.outputs.rows
            max: 2
            if: $steps.probe.outputs.row_count == 0
            steps:
              - id: gated
                uses: db/observe
                with: { connection: 'sqlite::memory:', sql: 'select 1', until: { row_count: 1 } }"#)
        .replacen("assertions: [\"true\"]", "assertions: [$steps.gated.outputs.satisfied == true]", 1);
    let (state, trace) = run(&yaml).await;
    assert_eq!(state, MISSING);
    assert!(trace.events().iter().any(|event| matches!(
        event.payload,
        EventPayload::CheckFinished {
            gated_judging_steps: 1,
            ..
        }
    )));
}
