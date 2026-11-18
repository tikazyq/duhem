use std::{collections::BTreeMap, sync::Arc};

use duhem_evidence::{
    EventPayload, ReplayDivergence, ReplayError, RunBundle, SecretRegistry, SqliteStore, Trace,
    replay,
};
use duhem_judge::{InconclusiveCause, VerdictState};
use duhem_runtime::Engine;
use duhem_schema::{InconclusivePolicy, RootManifest, VerificationDefinition};

const MANIFEST: &str = include_str!("../../../verifications/defaults-example/duhem.yml");
const LEAF: &str = include_str!("../../../verifications/defaults-example/checks/duhem.yml");
const MISSING: VerdictState = VerdictState::Inconclusive(InconclusiveCause::MissingObservation);

#[tokio::test]
async fn recorded_policy_replays_and_ingests_with_raw_checks_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let source = Arc::new(
        SqliteStore::open(tmp.path().join("source.db"))
            .await
            .unwrap(),
    );
    let destination = SqliteStore::open(tmp.path().join("destination.db"))
        .await
        .unwrap();
    let definition = VerificationDefinition::from_yaml_str(LEAF).unwrap();
    let mut defaults = RootManifest::from_yaml_str(MANIFEST)
        .unwrap()
        .defaults
        .unwrap();
    assert_eq!(defaults.inconclusive_policy, Some(InconclusivePolicy::Warn));

    for (policy, wire, expected, warnings) in [
        (InconclusivePolicy::Warn, "warn", VerdictState::Pass, 1),
        (InconclusivePolicy::Pass, "pass", VerdictState::Pass, 0),
        (InconclusivePolicy::Block, "block", MISSING, 0),
    ] {
        defaults.inconclusive_policy = Some(policy);
        let mut secrets = SecretRegistry::new();
        secrets.register("token", "warn");
        let live = Engine::new()
            .with_defaults(&defaults)
            .with_secret_registry(secrets)
            .with_store(source.clone())
            .run_with_metadata(&definition, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(live.verdict.state, expected);
        assert_eq!(live.verdict.criteria[1].checks[0].state, MISSING);
        assert_eq!(live.warnings.len(), warnings);

        let bundle = RunBundle::from_store(source.as_ref(), &live.run_id)
            .await
            .unwrap();
        let hash = bundle.content_hash().unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&bundle.wire_bytes().unwrap()).unwrap();
        let bundle: RunBundle = serde_json::from_value(json.clone()).unwrap();
        destination.ingest_bundle(&bundle, &hash).await.unwrap();
        assert_eq!(json["events"][0]["inconclusive_policy"], wire);
        let replayed = Trace::from_store(&destination, &live.run_id).await.unwrap();
        assert_eq!(replay(&replayed).unwrap().run, live.verdict);

        json["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("inconclusive_policy");
        let legacy: RunBundle = serde_json::from_value(json).unwrap();
        let legacy = Trace::from_events(legacy.events).unwrap();
        if policy == InconclusivePolicy::Block {
            assert_eq!(replay(&legacy).unwrap().run, live.verdict);
        } else {
            assert_eq!(
                replay(&legacy),
                Err(ReplayError::Divergence(ReplayDivergence::Criterion {
                    criterion_id: "AC-2".into(),
                    recorded: VerdictState::Pass,
                    recomputed: MISSING,
                }))
            );
        }
    }
}

#[tokio::test]
async fn warn_policy_rejects_tampered_verdicts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(tmp.path().join("evidence.db"))
            .await
            .unwrap(),
    );
    let defaults = RootManifest::from_yaml_str(MANIFEST)
        .unwrap()
        .defaults
        .unwrap();
    let definition =
        VerificationDefinition::from_yaml_str(&LEAF.replacen(r#"- "true""#, r#"- "false""#, 1))
            .unwrap();
    let live = Engine::new()
        .with_defaults(&defaults)
        .with_store(store.clone())
        .run_with_metadata(&definition, BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(live.verdict.state, VerdictState::Fail);
    let trace = Trace::from_store(store.as_ref(), &live.run_id)
        .await
        .unwrap();
    assert_eq!(replay(&trace).unwrap().run, live.verdict);

    let mut events = trace.events().to_vec();
    for event in &mut events {
        if let EventPayload::CheckFinished {
            check_id, verdict, ..
        } = &mut event.payload
            && check_id == "AC-2.1"
        {
            *verdict = VerdictState::Pass;
        }
    }
    assert_eq!(
        replay(&Trace::from_events(events).unwrap()),
        Err(ReplayError::Divergence(ReplayDivergence::Check {
            check_id: "AC-2.1".into(),
            recorded: VerdictState::Pass,
            recomputed: MISSING,
        }))
    );

    let mut events = trace.into_events();
    for event in &mut events {
        if let EventPayload::CriterionFinished {
            criterion_id,
            verdict,
        } = &mut event.payload
            && criterion_id == "AC-1"
        {
            *verdict = VerdictState::Pass;
        }
    }
    assert_eq!(
        replay(&Trace::from_events(events).unwrap()),
        Err(ReplayError::Divergence(ReplayDivergence::Criterion {
            criterion_id: "AC-1".into(),
            recorded: VerdictState::Pass,
            recomputed: VerdictState::Fail,
        }))
    );
}
