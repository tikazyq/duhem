// Engine errors seal partial evidence after cleanup without inventing a verdict.
#[tokio::test]
async fn engine_abort_seals_after_cleanup_and_preserves_completed_judgments() {
    let (mut engine, _tmp) = engine_for_test().await;
    engine.register_test_action(Box::new(StubAction::new("fake/ok", Outcome::Ok)));
    let definition = load_flows(
        r#"
verification: aborted evidence
teardown: [{uses: fake/ok}]
flows:
  resource:
    steps:
      - id: make
        uses: fake/ok
    cleanup:
      - uses: fake/ok
        with: {value: $steps.make.outputs.value}
      - uses: fake/ok
        if: always
criteria:
  - id: complete
    description: completed criterion
    checks:
      - id: complete-check
        assertions: ["true"]
  - id: open
    description: unfinished criterion
    checks:
      - id: checked
        assertions: ["true"]
      - id: abort
        steps:
          - call: resource
          - id: missing
            uses: fake/ok
            if: failure
          - uses: fake/ok
            with: {value: $steps.missing.outputs.value}
        teardown: [{uses: fake/ok}]
        assertions: ["true"]
"#,
    );
    let error = engine.run(&definition, BTreeMap::new()).await.unwrap_err();
    assert!(
        matches!(&error, EngineError::UnresolvedReference { reference, .. }
        if reference == "$steps.missing.outputs.value")
    );
    let events = read_only_run_events(&engine).await;
    assert!(matches!(&events.last().unwrap().payload,
        EventPayload::RunAborted { signal, error: Some(detail) }
        if signal == "engine_error" && detail == &error.to_string()));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::RunAborted { .. } | EventPayload::RunFinished { .. }
            ))
            .count(),
        1
    );
    assert!(
        events[..events.len() - 1]
            .iter()
            .rev()
            .any(|event| matches!(
                event.payload,
                EventPayload::SetupFinished {
                    phase: duhem_evidence::StepPhase::Teardown,
                    ..
                }
            ))
    );
    assert!(events.iter().any(|event| matches!(&event.payload,
        EventPayload::SetupStepFinished {
            phase: duhem_evidence::StepPhase::Teardown,
            outcome: duhem_evidence::StepOutcome::Error,
            detail: Some(detail), ..
        } if detail.contains("outputs.value") && detail.contains("make"))));
    let trace = Trace::from_events(events).unwrap();
    duhem_evidence::replay_aborted(&trace).unwrap();
    assert_eq!(
        duhem_evidence::replay(&trace),
        Err(duhem_evidence::ReplayError::MissingRunFinished)
    );
    let runs = engine.store.as_ref().unwrap().list_runs().await.unwrap();
    assert_eq!(runs[0].status, duhem_evidence::RunStatus::Aborted);
    assert_eq!(runs[0].verdict, None);
}

#[tokio::test]
async fn engine_abort_masks_error_using_acquired_secret_registry() {
    let (mut engine, _tmp) = engine_for_test().await;
    let secret = "acquired-secret-in-expression";
    engine.register_test_action(Box::new(
        StubAction::new("fake/login", Outcome::Ok).with_output("token", serde_json::json!(secret)),
    ));
    engine.register_test_action(Box::new(StubAction::new("fake/ok", Outcome::Ok)));
    let definition = def(&format!(
        r#"
verification: masked abort
criteria:
  - id: AC
    description: acquired secret
    checks:
      - id: c
        steps:
          - id: login
            uses: fake/login
            secret_outputs: [token]
          - uses: fake/ok
            with: {{value: '$runtime.format("{secret}/{{}}", $steps.missing.outputs.value)'}}
        assertions: ["true"]
"#
    ));
    let error = engine.run(&definition, BTreeMap::new()).await.unwrap_err();
    assert!(
        error.to_string().contains(secret),
        "original error preserved"
    );
    let events = read_only_run_events(&engine).await;
    let EventPayload::RunAborted {
        error: Some(detail),
        ..
    } = &events.last().unwrap().payload
    else {
        panic!("missing abort error");
    };
    assert!(!detail.contains(secret));
    assert!(detail.contains("[redacted:login.token]"), "{detail}");
}
