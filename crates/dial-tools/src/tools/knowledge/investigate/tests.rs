use super::*;
use crate::runtime::evidence::{Coverage, CoverageExtent};
use crate::tools::testkit;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}
fn source(ws: &Workspace, text: &str) -> String {
    let item = Evidence {
        source: "fixture.txt".into(),
        location: Location::Whole,
        method: Method::Read,
        producer: "test".into(),
        kind: EvidenceKind::Observation,
        summary: "fixture".into(),
        representation: Representation::of_bytes("text/plain", text.as_bytes()),
        limitations: vec![],
        context: EvidenceContext {
            coverage: Some(Coverage {
                extent: CoverageExtent::Complete,
                inspected: vec![Location::Whole],
                omitted: vec![],
                truncated: false,
                failures: vec![],
            }),
            ..Default::default()
        },
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&item, text.as_bytes())
        .unwrap()
}
fn citation(out: &ToolOutput) -> &str {
    out.data.as_ref().unwrap()["evidence_references"][0]
        .as_str()
        .unwrap()
}
fn hypotheses() -> Value {
    json!([
        {"id":"a","explanation":"cache is stale","test":"disable cache and rerun","prediction":"failure disappears","falsifier":"same failure without cache"},
        {"id":"b","explanation":"input is invalid","test":"validate input independently","prediction":"validator rejects it","falsifier":"validator accepts it"}
    ])
}
#[test]
fn compare_retains_inputs_and_interpret_checks_quotes_and_attribution() {
    let (_work, _state, ws) = fixture();
    let a = source(&ws, "timeout = 5\n");
    let b = source(&ws, "timeout = 10\n");
    let out = run(&ws, &json!({"action":"compare","reference":a,"other":b})).unwrap();
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(citation(&out))
        .unwrap();
    assert_eq!(record.evidence.context.parents, vec![a.clone(), b.clone()]);
    assert!(
        out.data.as_ref().unwrap()["text_diff"]
            .as_str()
            .unwrap()
            .contains("+timeout = 10")
    );
    let args = json!({"action":"interpret","reference":a,"other":b,"findings":[{"claim":"timeout doubled","left_quote":"timeout = 5","right_quote":"timeout = 10"}]});
    let out = run(&ws, &args).unwrap();
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(citation(&out))
        .unwrap();
    assert_eq!(record.evidence.kind, EvidenceKind::Inference);
    assert_eq!(record.evidence.method, Method::ModelInterpretation);
    let mut bad = args;
    bad["findings"][0]["right_quote"] = json!("invented");
    assert!(run(&ws, &bad).is_err());
}
#[test]
fn plans_are_falsifiable_records_and_results_require_prior_id_and_observation_citations() {
    let (_work, _state, ws) = fixture();
    let plan = run(
        &ws,
        &json!({"action":"plan","question":"Why does the request fail?","hypotheses":hypotheses()}),
    )
    .unwrap();
    let reference = citation(&plan);
    let basis = source(&ws, "validator accepted input");
    let mut args = json!({"action":"result","reference":reference,"hypothesis":"b","observation":"validator accepted input","assessment":"contradicts","parents":[basis]});
    let out = run(&ws, &args).unwrap();
    assert_eq!(out.data.as_ref().unwrap()["verification_credit"], false);
    assert_eq!(
        ws.artifacts()
            .unwrap()
            .evidence_record(citation(&out))
            .unwrap()
            .evidence
            .context
            .parents
            .len(),
        2
    );
    args["hypothesis"] = json!("absent");
    assert!(run(&ws, &args).is_err());
    args["hypothesis"] = json!("a");
    args["parents"] = json!([reference]);
    assert!(run(&ws, &args).is_err());
    let unrelated_plan = run(
        &ws,
        &json!({"action":"plan","question":"Another question?","hypotheses":hypotheses()}),
    )
    .unwrap();
    args["parents"] = json!([citation(&unrelated_plan)]);
    assert!(
        run(&ws, &args).is_err(),
        "another inference must not substitute for observation evidence"
    );
}
#[test]
fn rejects_non_competing_duplicate_or_unfalsifiable_plans_and_oversized_comparison() {
    let (_work, _state, ws) = fixture();
    let mut hs = hypotheses();
    hs[1]["id"] = json!("a");
    assert!(
        run(
            &ws,
            &json!({"action":"plan","question":"q","hypotheses":hs})
        )
        .is_err()
    );
    hs = hypotheses();
    hs[0]["falsifier"] = json!("");
    assert!(
        run(
            &ws,
            &json!({"action":"plan","question":"q","hypotheses":hs})
        )
        .is_err()
    );
    let a = source(&ws, &"x".repeat(49 * 1024));
    let b = source(&ws, "b");
    assert!(run(&ws, &json!({"action":"compare","reference":a,"other":b})).is_err());
}
#[test]
fn failed_citation_cannot_be_attached_to_an_experiment() {
    let (_work, _state, ws) = fixture();
    assert!(run(&ws,&json!({"action":"plan","question":"q","hypotheses":hypotheses(),"parents":[format!("artifact://{}","0".repeat(64))]})).is_err());
}
