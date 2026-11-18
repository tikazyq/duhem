//! Live judgment and bundle ingest must apply the same criterion policy.
use std::{collections::BTreeMap, sync::Arc};

use duhem_evidence::{
    Event, EventPayload, ReplayDivergence, ReplayError, RunBundle, SqliteStore, Trace, replay,
};
use duhem_judge::{InconclusiveCause, InconclusivePolicy, VerdictState};
use duhem_runtime::{Engine, RunOutcome};
use duhem_schema::{ManifestDefaults, VerificationDefinition};

const MISSING: VerdictState = VerdictState::Inconclusive(InconclusiveCause::MissingObservation);

async fn run(policy: &str, definition: &str) -> (RunOutcome, RunBundle) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(tmp.path().join("source.db"))
            .await
            .unwrap(),
    );
    let defaults: ManifestDefaults =
        serde_yml::from_str(&format!("inconclusive_policy: {policy}")).unwrap();
    let definition = VerificationDefinition::from_yaml_str(definition).unwrap();
    let outcome = Engine::new()
        .with_store(store.clone())
        .with_defaults(&defaults)
        .run_with_metadata(&definition, BTreeMap::new())
        .await
        .unwrap();
    let bundle = RunBundle::from_store(store.as_ref(), &outcome.run_id)
        .await
        .unwrap();
    (outcome, bundle)
}

fn replay_events(events: Vec<Event>) -> Result<duhem_evidence::ReplayedRun, ReplayError> {
    replay(&Trace::from_events(events).unwrap())
}

// Remove the wire field, rather than synthesizing a policy from a verdict or
// source snapshot. This also exercises deserialization of pre-policy evidence.
fn without_policy(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            value.as_object_mut().unwrap().remove("inconclusive_policy");
            serde_json::from_value(value).unwrap()
        })
        .collect()
}

#[test]
fn policy_wire_values_are_explicit_and_unknown_values_are_rejected() {
    let header = serde_json::json!({
        "kind": "run_started",
        "verification_path": "checks.yml",
        "schema_version": "v1"
    });
    for (wire, expected) in [
        ("block", InconclusivePolicy::Block),
        ("warn", InconclusivePolicy::Warn),
        ("pass", InconclusivePolicy::Pass),
    ] {
        let mut value = header.clone();
        value["inconclusive_policy"] = serde_json::json!(wire);
        let payload: EventPayload = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(payload,
            EventPayload::RunStarted { inconclusive_policy: Some(p), .. } if p == expected
        ));
        assert_eq!(
            serde_json::to_value(payload).unwrap()["inconclusive_policy"],
            wire
        );
    }
    let mut value = header;
    value["inconclusive_policy"] = serde_json::Value::Null;
    let payload: EventPayload = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(
        payload,
        EventPayload::RunStarted {
            inconclusive_policy: None,
            ..
        }
    ));
    assert!(
        serde_json::to_value(payload)
            .unwrap()
            .get("inconclusive_policy")
            .is_none()
    );
    value["inconclusive_policy"] = serde_json::json!("unknown");
    assert!(serde_json::from_value::<EventPayload>(value).is_err());
}

#[tokio::test]
async fn defaults_example_replays_and_ingests_under_each_policy() {
    let definition = include_str!("../../../verifications/defaults-example/checks/duhem.yml");
    for (policy, recorded_policy, expected) in [
        ("block", InconclusivePolicy::Block, MISSING),
        ("warn", InconclusivePolicy::Warn, VerdictState::Pass),
        ("pass", InconclusivePolicy::Pass, VerdictState::Pass),
    ] {
        let (outcome, bundle) = run(policy, definition).await;
        assert_eq!(outcome.verdict.state, expected);
        assert_eq!(outcome.warnings.len(), usize::from(policy == "warn"));
        assert!(matches!(
            bundle.events[0].payload,
            EventPayload::RunStarted { inconclusive_policy: Some(p), .. } if p == recorded_policy
        ));
        assert!(bundle.events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::CheckFinished { check_id, verdict, .. }
                if check_id == "AC-2.1" && *verdict == MISSING
        )));
        assert_eq!(
            replay_events(bundle.events.clone()).unwrap().run,
            outcome.verdict
        );

        // Exercise the same replay gate used by dashboard ingest after a wire
        // round-trip, with real SQLite stores on both sides.
        let wire = bundle.wire_bytes().unwrap();
        let received: RunBundle = serde_json::from_slice(&wire).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let destination = SqliteStore::open(tmp.path().join("destination.db"))
            .await
            .unwrap();
        destination
            .ingest_bundle(&received, &received.content_hash().unwrap())
            .await
            .unwrap();
        let stored = Trace::from_store(&destination, &outcome.run_id)
            .await
            .unwrap();
        assert_eq!(replay(&stored).unwrap().run, outcome.verdict);

        let legacy = without_policy(&bundle.events);
        if policy == "block" {
            assert_eq!(replay_events(legacy).unwrap().run, outcome.verdict);
        } else {
            assert!(matches!(
                replay_events(legacy),
                Err(ReplayError::Divergence(ReplayDivergence::Criterion {
                    criterion_id, recorded: VerdictState::Pass, recomputed: MISSING,
                })) if criterion_id == "AC-2"
            ));
        }
    }
}

#[tokio::test]
async fn softened_policy_still_rejects_raw_check_and_criterion_and_run_divergence() {
    let definition = include_str!("../../../verifications/defaults-example/checks/duhem.yml");
    for policy in ["warn", "pass"] {
        let (_, bundle) = run(policy, definition).await;
        let mut checks = bundle.events.clone();
        for event in &mut checks {
            if let EventPayload::CheckFinished {
                check_id, verdict, ..
            } = &mut event.payload
                && check_id == "AC-2.1"
            {
                *verdict = VerdictState::Pass;
            }
        }
        assert!(matches!(
            replay_events(checks.clone()),
            Err(ReplayError::Divergence(ReplayDivergence::Check {
                recorded: VerdictState::Pass,
                recomputed: MISSING,
                ..
            }))
        ));
        let mut tampered = bundle.clone();
        tampered.events = checks;
        let tmp = tempfile::tempdir().unwrap();
        let destination = SqliteStore::open(tmp.path().join("destination.db"))
            .await
            .unwrap();
        assert!(
            destination
                .ingest_bundle(&tampered, &tampered.content_hash().unwrap())
                .await
                .is_err()
        );

        let mut criteria = bundle.events.clone();
        for event in &mut criteria {
            if let EventPayload::CriterionFinished {
                criterion_id,
                verdict,
            } = &mut event.payload
                && criterion_id == "AC-2"
            {
                *verdict = MISSING;
            }
        }
        assert!(matches!(
            replay_events(criteria),
            Err(ReplayError::Divergence(ReplayDivergence::Criterion {
                recorded: MISSING,
                recomputed: VerdictState::Pass,
                ..
            }))
        ));

        let mut run = bundle.events.clone();
        for event in &mut run {
            if let EventPayload::RunFinished { verdict, .. } = &mut event.payload {
                *verdict = Some(VerdictState::Fail);
            }
        }
        assert!(matches!(
            replay_events(run),
            Err(ReplayError::Divergence(ReplayDivergence::Run {
                recorded: VerdictState::Fail,
                recomputed: VerdictState::Pass,
            }))
        ));

        let mut body = bundle.events.clone();
        for event in &mut body {
            if let EventPayload::RunFinished { body_verdict, .. } = &mut event.payload {
                *body_verdict = Some(VerdictState::Fail);
            }
        }
        assert!(matches!(
            replay_events(body),
            Err(ReplayError::Divergence(ReplayDivergence::Body {
                recorded: VerdictState::Fail,
                recomputed: VerdictState::Pass,
            }))
        ));
    }
}

#[tokio::test]
async fn real_failure_never_softens_and_a_tampered_passing_criterion_is_rejected() {
    let definition = r#"
verification: real failure
criteria:
  - id: AC-1
    description: A failed assertion is never softened
    checks:
      - id: AC-1.1
        assertions: ["false"]
      - id: AC-1.2
        steps:
          - uses: nope/unknown
        assertions: ["true"]
"#;
    for policy in ["block", "warn", "pass"] {
        let (outcome, bundle) = run(policy, definition).await;
        assert_eq!(outcome.verdict.state, VerdictState::Fail);
        assert!(outcome.warnings.is_empty());
        assert_eq!(
            replay_events(bundle.events.clone()).unwrap().run,
            outcome.verdict
        );
        assert_eq!(
            replay_events(without_policy(&bundle.events)).unwrap().run,
            outcome.verdict
        );
        let mut tampered = bundle;
        for event in &mut tampered.events {
            match &mut event.payload {
                EventPayload::CriterionFinished { verdict, .. } => *verdict = VerdictState::Pass,
                EventPayload::RunFinished { verdict, .. } => *verdict = Some(VerdictState::Pass),
                _ => {}
            }
        }
        assert!(matches!(
            replay_events(tampered.events.clone()),
            Err(ReplayError::Divergence(ReplayDivergence::Criterion {
                recorded: VerdictState::Pass,
                recomputed: VerdictState::Fail,
                ..
            }))
        ));
        tampered.run.verdict = Some(VerdictState::Pass);
        let tmp = tempfile::tempdir().unwrap();
        let destination = SqliteStore::open(tmp.path().join("destination.db"))
            .await
            .unwrap();
        assert!(
            destination
                .ingest_bundle(&tampered, &tampered.content_hash().unwrap())
                .await
                .is_err()
        );
    }
}
