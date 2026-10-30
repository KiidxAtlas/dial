use super::*;
use crate::{storage::artifact::ArtifactStore, tools::testkit};
fn retain(ws: &Workspace, text: &str, kind: EvidenceKind) -> String {
    let item = Evidence {
        source: "fixture".into(),
        location: Location::Whole,
        method: Method::Read,
        producer: "fixture".into(),
        kind,
        summary: "fixture".into(),
        representation: Representation::of_bytes("text/plain", text.as_bytes()),
        limitations: vec![],
        context: EvidenceContext::default(),
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&item, text.as_bytes())
        .unwrap()
}
#[test]
fn checks_real_retained_counts_and_links_expected_mapping_without_verification() {
    let (_work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let expected = retain(
        &ws,
        "Retries at most three times",
        EvidenceKind::Observation,
    );
    let observed = retain(&ws, "retry\nretry\nretry\nretry", EvidenceKind::Observation);
    let args = json!({"action":"check","expectation":expected,"observation":observed,"expectation_quote":"at most three","claim":"At most three retry log markers","expected":3,"selector":{"kind":"count","value":"retry"},"operator":"le"});
    let out = run(&ws, &args).unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["measurement"]["actual"], 4);
    assert_eq!(data["measurement"]["matches_caller_expectation"], false);
    assert_eq!(data["verification_credit"], false);
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(record.evidence.context.parents, vec![expected, observed]);
    assert_eq!(record.evidence.kind, EvidenceKind::Inference);
    let mut bad = args.clone();
    bad["expectation_quote"] = json!("invented");
    assert!(run(&ws, &bad).is_err());
    let inference = retain(&ws, "retry", EvidenceKind::Inference);
    bad = args;
    bad["observation"] = json!(inference);
    assert!(run(&ws, &bad).is_err());
}
#[test]
fn pointers_preserve_precision_and_reject_missing_or_structured_values() {
    let (_work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let expected = retain(&ws, "expected count", EvidenceKind::Observation);
    let observed = retain(
        &ws,
        r#"{"count":9007199254740993.0,"ok":2,"nested":{}}"#,
        EvidenceKind::Observation,
    );
    let mut args = json!({"action":"check","expectation":expected,"observation":observed,"expectation_quote":"expected count","claim":"count matches","expected":2,"selector":{"kind":"pointer","value":"/ok"}});
    assert_eq!(
        run(&ws, &args).unwrap().data.unwrap()["measurement"]["matches_caller_expectation"],
        true
    );
    for pointer in ["/count", "/absent", "/nested"] {
        args["selector"]["value"] = json!(pointer);
        assert!(run(&ws, &args).is_err(), "accepted {pointer}");
    }
}

#[test]
fn explicit_null_is_a_scalar_but_missing_expected_is_an_error() {
    let (_work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let expected = retain(&ws, "value can be null", EvidenceKind::Observation);
    let observed = retain(&ws, r#"{"value":null}"#, EvidenceKind::Observation);
    let mut args = json!({"action":"check","expectation":expected,"observation":observed,"expectation_quote":"can be null","claim":"value is null","expected":null,"selector":{"kind":"pointer","value":"/value"}});
    let output = run(&ws, &args).unwrap().data.unwrap();
    assert_eq!(output["measurement"]["matches_caller_expectation"], true);
    args.as_object_mut().unwrap().remove("expected");
    assert!(
        run(&ws, &args)
            .unwrap_err()
            .to_string()
            .contains("expected scalar")
    );
}
