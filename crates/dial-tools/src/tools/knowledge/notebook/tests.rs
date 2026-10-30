use super::*;
use crate::tools::testkit;
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}
fn source(ws: &Workspace, text: &str) -> String {
    let e = Evidence {
        source: "fixture".into(),
        location: Location::Whole,
        method: Method::Read,
        producer: "fixture".into(),
        kind: EvidenceKind::Observation,
        summary: "fixture".into(),
        representation: Representation::of_bytes("text/plain", text.as_bytes()),
        limitations: vec![],
        context: EvidenceContext::default(),
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&e, text.as_bytes())
        .unwrap()
}
fn citation(out: &ToolOutput) -> String {
    out.data.as_ref().unwrap()["evidence_references"][0]
        .as_str()
        .unwrap()
        .into()
}
#[test]
fn persisted_restart_resumes_prior_results_without_verification_or_reexecution() {
    let (_work, state, ws) = fixture();
    let a = source(&ws, "result output");
    let first=run(&ws,&json!({"action":"create","question":"Why?","entries":[{"kind":"result","summary":"Caller says test failed","references":[a]}],"competing_explanations":["cache","input"],"next_experiment":"Discriminate via investigate plan"})).unwrap();
    let predecessor = citation(&first);
    let second=run(&ws,&json!({"action":"update","reference":predecessor,"entries":[{"kind":"correction","summary":"Caller revises reading","references":[a],"contradicts":[a]}],"unknowns":["root cause"]})).unwrap();
    let reference = citation(&second);
    drop(ws);
    let (_work2, ws) = testkit::workspace();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let resumed = run(&ws, &json!({"action":"resume","reference":reference})).unwrap();
    let d = resumed.data.unwrap();
    assert_eq!(d["revision"], 2);
    assert_eq!(d["entries"].as_array().unwrap().len(), 2);
    assert_eq!(d["predecessor"], predecessor);
    assert_eq!(d["verification_credit"], false);
    assert_eq!(d["retained_tests_results"].as_array().unwrap().len(), 1);
    let record = ws.artifacts().unwrap().evidence_record(&reference).unwrap();
    assert!(record.evidence.context.parents.contains(&a));
    assert!(record.evidence.context.parents.contains(&predecessor));
    let original = run(&ws, &json!({"action":"resume","reference":predecessor})).unwrap();
    assert_eq!(
        original.data.unwrap()["entries"].as_array().unwrap().len(),
        1
    );
}
#[test]
fn arbitrary_json_cannot_impersonate_notebook_and_missing_dependencies_fail() {
    let (_w, _s, ws) = fixture();
    let fake = source(&ws, r#"{"type":"research_notebook","revision":1}"#);
    assert!(run(&ws, &json!({"action":"resume","reference":fake})).is_err());
    assert!(run(&ws,&json!({"action":"create","question":"Why?","entries":[{"kind":"observation","summary":"missing","references":["artifact://unknown"]}]})).is_err());
}
#[test]
fn bounded_history_and_entries_refuse_growth_without_changing_prior_snapshots() {
    let (_w, _s, ws) = fixture();
    let first = run(&ws, &json!({"action":"create","question":"Why?"})).unwrap();
    let mut reference = citation(&first);
    for _ in 1..MAX_HISTORY {
        reference = citation(&run(&ws, &json!({"action":"update","reference":reference})).unwrap());
    }
    assert!(run(&ws, &json!({"action":"update","reference":reference})).is_err());
    assert_eq!(
        run(&ws, &json!({"action":"resume","reference":reference}))
            .unwrap()
            .data
            .unwrap()["revision"],
        MAX_HISTORY
    );
    assert!(
        run(
            &ws,
            &json!({"action":"create","question":"Why?","unknowns":vec!["unknown";17]})
        )
        .is_err()
    );
    assert!(run(&ws, &json!({"action":"create","question":"x".repeat(4097)})).is_err());
}

#[test]
fn media_payloads_larger_than_snapshot_cap_authenticate_with_shared_budget() {
    let (_w, _s, ws) = fixture();
    let bytes = vec![42; 256 * 1024];
    let item = Evidence {
        source: "screen".into(),
        location: Location::Whole,
        method: Method::Render,
        producer: "screenshot".into(),
        kind: EvidenceKind::Observation,
        summary: "retained pixels".into(),
        representation: Representation::of_bytes("image/png", &bytes),
        limitations: vec![],
        context: EvidenceContext::default(),
    };
    let reference = ws
        .artifacts()
        .unwrap()
        .retain_evidence(&item, &bytes)
        .unwrap();
    let out = run(&ws,&json!({"action":"create","question":"What changed?","entries":[{"kind":"observation","summary":"Caller sees a change","references":[reference]}]})).unwrap();
    let notebook = citation(&out);
    assert!(run(&ws, &json!({"action":"resume","reference":notebook})).is_ok());
    let mut auth = Authentication {
        store: ws.artifacts().unwrap(),
        remaining: bytes.len(),
        seen: BTreeSet::new(),
    };
    auth.reference(&reference).unwrap();
    assert_eq!(auth.remaining, 0);
    auth.reference(&reference).unwrap(); // Deduplication does not spend the budget twice.
    let other = source(&ws, "different payload");
    assert!(auth.reference(&other).is_err());
}
