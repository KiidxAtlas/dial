use super::*;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}
fn retain(
    ws: &Workspace,
    media: &str,
    bytes: &[u8],
    location: Location,
    kind: EvidenceKind,
) -> String {
    let evidence = Evidence {
        source: "not-reopened.txt".into(),
        location,
        method: Method::Read,
        producer: "fixture".into(),
        kind,
        summary: "fixture".into(),
        representation: Representation::of_bytes(media, bytes),
        limitations: vec!["retained sample".into()],
        context: EvidenceContext::default(),
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&evidence, bytes)
        .unwrap()
}
fn find(ws: &Workspace, value: Value) -> Retrieved {
    retrieve(ws, &arguments(&value).unwrap(), &AtomicBool::new(false)).unwrap()
}
#[test]
fn mixed_sources_preserve_quotes_values_coordinates_and_manifests() {
    let (_work, _state, ws) = fixture();
    let a = retain(
        &ws,
        "text/plain",
        b"first\nneedle exact\r\n",
        Location::Page { number: 3 },
        EvidenceKind::Observation,
    );
    let b=retain(&ws,"application/json",br#"{"segments":[{"start_ms":120,"end_ms":380,"text":"needle speech"}],"rows":[{"a/b":42}]}"#,Location::Time{start_ms:0,end_ms:1000},EvidenceKind::Inference);
    let found = find(&ws, json!({"references":[a,b],"query":"needle 42"}));
    let out = found
        .render(
            found.hits.iter().filter(|h| h.lexical_points > 0).collect(),
            "lexical",
        )
        .unwrap();
    assert_eq!(out["hits"][0]["quote"], "needle exact\r\n");
    assert_eq!(out["hits"][0]["representation_location"]["line"], 2);
    let speech = found
        .hits
        .iter()
        .find(|h| h.quote == "needle speech")
        .unwrap();
    assert_eq!(
        speech.representation_location["json_pointer"],
        "/segments/0/text"
    );
    assert_eq!(
        speech.representation_location["ancestor_metadata"]["start_ms"],
        120
    );
    let value = found
        .hits
        .iter()
        .find(|h| h.value == Some(json!(42)))
        .unwrap();
    assert_eq!(
        value.representation_location["json_pointer"],
        "/rows/0/a~1b"
    );
    assert_eq!(out["manifests"][&b]["evidence"]["kind"], "inference");
    assert_eq!(out["manifests"][&a]["evidence"]["location"]["number"], 3);
    assert_eq!(
        out["coverage"]["freshness"],
        "unknown: original sources not reopened"
    );
}
#[test]
fn semantic_selects_lexical_misses_and_rejects_invalid_selections() {
    let (_work, _state, ws) = fixture();
    let a = retain(
        &ws,
        "text/plain",
        b"automobile journey\n",
        Location::Whole,
        EvidenceKind::Observation,
    );
    let args = json!({"references":[a],"query":"car travel","ranking":"semantic","limit":2});
    let request = semantic_request(&ws, &args).unwrap().unwrap();
    assert!(request.has_candidates());
    let out: Value =
        serde_json::from_str(&request.render_response(r#"{"indices":[0]}"#).unwrap()).unwrap();
    assert_eq!(out["hits"][0]["quote"], "automobile journey\n");
    assert_eq!(out["hits"][0]["lexical_points"], 0);
    for response in [
        r#"{"indices":[999]}"#,
        r#"{"indices":[0,0]}"#,
        r#"{"indices":[-1]}"#,
        r#"{"indices":[0.5]}"#,
        r#"{"indices":[],"citation":"invented"}"#,
    ] {
        assert!(request.render_response(response).is_err(), "{response}");
    }
    assert!(
        semantic_request(&ws, &json!({"references":[a],"query":"car"}))
            .unwrap()
            .is_none()
    );
}
#[test]
fn explicit_gaps_for_missing_tampered_binary_and_oversize_artifacts() {
    let (_work, state, ws) = fixture();
    let a = retain(
        &ws,
        "text/plain",
        b"needle",
        Location::Whole,
        EvidenceKind::Observation,
    );
    let record = ws.artifacts().unwrap().evidence_record(&a).unwrap();
    std::fs::write(
        state.path().join(
            record
                .representation_uri
                .strip_prefix("artifact://")
                .unwrap(),
        ),
        b"evil",
    )
    .unwrap();
    let binary = retain(
        &ws,
        "image/png",
        b"binary",
        Location::Whole,
        EvidenceKind::Observation,
    );
    let large = retain(
        &ws,
        "text/plain",
        &vec![b'x'; MAX_BYTES + 1],
        Location::Whole,
        EvidenceKind::Observation,
    );
    let absent = format!("artifact://{}", "0".repeat(64));
    let found = find(
        &ws,
        json!({"references":[absent,binary,large,a],"query":"needle"}),
    );
    assert_eq!(found.gaps.len(), 4);
    assert!(found.hits.is_empty());
    assert!(
        found
            .gaps
            .iter()
            .any(|g| g["reason"].as_str().unwrap().contains("hash mismatch"))
    );
}
#[test]
fn budgets_and_cancellation_are_enforced() {
    let (_work, _state, ws) = fixture();
    let a = retain(
        &ws,
        "text/plain",
        &"needle\n".repeat(5000).into_bytes(),
        Location::Whole,
        EvidenceKind::Observation,
    );
    let value = json!({"references":[a],"query":"needle"});
    let found = find(&ws, value.clone());
    assert!(found.hits.len() <= 8);
    assert!(!found.gaps.is_empty());
    assert!(found.inspected <= 4096);
    assert!(retrieve(&ws, &arguments(&value).unwrap(), &AtomicBool::new(true)).is_err());
    assert!(
        chunks::parse(
            "a",
            "text/plain",
            b"hello",
            &["hello".into()],
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert!(arguments(&json!({"references":[],"query":"needle"})).is_err());
    assert!(arguments(&json!({"references":[a],"query":"needle","limit":21})).is_err());
    assert!(arguments(&json!({"references":[a],"query":"x".repeat(4097)})).is_err());
}
