use super::super::evidence::source;
use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}
fn source_record(ws: &Workspace, text: &str, parents: Vec<String>) -> String {
    let item = Evidence {
        source: "fixture.txt".into(),
        location: Location::Whole,
        method: Method::Read,
        producer: "test".into(),
        kind: EvidenceKind::Observation,
        summary: "fixture".into(),
        representation: Representation::of_bytes("text/plain", text.as_bytes()),
        limitations: vec!["test limitation".into()],
        context: EvidenceContext {
            parents,
            snapshot: Some(source::snapshot(text.as_bytes()).unwrap()),
            ..Default::default()
        },
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&item, text.as_bytes())
        .unwrap()
}
#[test]
fn lineage_reports_conflicting_revisions_and_never_reopens_sources() {
    let (work, _state, ws) = fixture();
    let a = source_record(&ws, "before", vec![]);
    let b = source_record(&ws, "after", vec![a]);
    std::fs::write(
        work.path().join("fixture.txt"),
        "mutable data not to be shown",
    )
    .unwrap();
    let out = run(&ws, &json!({"action":"lineage","references":[b]})).unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(data["conflicting_snapshots"].as_array().unwrap().len(), 1);
    assert!(!out.text.contains("mutable data not to be shown"));
}
#[test]
fn unavailable_citations_and_depth_gaps_are_visible() {
    let (_work, _state, ws) = fixture();
    let a = source_record(&ws, "before", vec![]);
    let b = source_record(&ws, "after", vec![a.clone()]);
    let out = run(&ws, &json!({"action":"lineage","references":[b],"depth":0})).unwrap();
    assert_eq!(out.data.unwrap()["omitted_references"], json!([a]));
    let missing = format!("artifact://{}", "0".repeat(64));
    let out = run(&ws, &json!({"action":"inspect","references":[missing]})).unwrap();
    assert_eq!(
        out.data.unwrap()["nodes"][0]["status"],
        "unavailable_or_invalid"
    );
}
#[test]
fn correction_is_append_only_parent_linked_and_not_automatically_human() {
    let (_work, _state, ws) = fixture();
    let a = source_record(&ws, "before", vec![]);
    let old = ws.artifacts().unwrap().evidence_record(&a).unwrap();
    let out = run(
        &ws,
        &json!({"action":"correct","references":[a],"correction":"The event happened later"}),
    )
    .unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["supersedes_in_place"], false);
    assert_eq!(data["attribution"], "caller; no human approval inferred");
    let record = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(record.evidence.kind, EvidenceKind::Inference);
    assert_eq!(record.evidence.context.parents, vec![a.clone()]);
    assert_eq!(old, ws.artifacts().unwrap().evidence_record(&a).unwrap());
}
#[test]
fn terminal_preview_sanitizes_controls_and_bounds_unicode() {
    let (_work, _state, ws) = fixture();
    let a = source_record(
        &ws,
        &format!("\u{1b}[2J\u{202e}{}", "é".repeat(4096)),
        vec![],
    );
    let out = run(&ws, &json!({"action":"inspect","references":[a]})).unwrap();
    assert!(!out.text.contains('\u{1b}'));
    assert!(!out.text.contains('\u{202e}'));
    assert_eq!(out.data.unwrap()["nodes"][0]["preview"]["truncated"], true);
}

#[test]
fn missing_retained_manifest_is_distinguished_from_unknown_reference() {
    let (_work, state, ws) = fixture();
    let reference = source_record(&ws, "retained", vec![]);
    std::fs::remove_file(
        state
            .path()
            .join(reference.strip_prefix("artifact://").unwrap()),
    )
    .unwrap();
    let out = run(&ws, &json!({"action":"inspect","references":[reference]})).unwrap();
    assert_eq!(
        out.data.unwrap()["nodes"][0]["manifest_retention"]["state"],
        "missing_after_retention"
    );
}

#[test]
fn exhausted_representation_budget_is_visible_and_lineage_node_limit_is_honest() {
    let (_work, _state, ws) = fixture();
    let large = source_record(&ws, &"x".repeat(MAX_BYTES + 1), vec![]);
    let small = source_record(&ws, "small", vec![]);
    let out = run(
        &ws,
        &json!({"action":"inspect","references":[large, small]}),
    )
    .unwrap();
    let data = out.data.unwrap();
    assert_eq!(
        data["nodes"][1]["representation_status"],
        "not_read_budget_exhausted"
    );
    let mut roots = Vec::new();
    for i in 0..16 {
        let parents = (0..4)
            .map(|j| source_record(&ws, &format!("leaf {i} {j}"), vec![]))
            .collect();
        roots.push(source_record(&ws, &format!("root {i}"), parents));
    }
    let out = run(
        &ws,
        &json!({"action":"lineage","references":roots,"depth":1}),
    )
    .unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["nodes"].as_array().unwrap().len(), MAX_NODES);
    assert_eq!(data["omitted_references"].as_array().unwrap().len(), 16);
}
