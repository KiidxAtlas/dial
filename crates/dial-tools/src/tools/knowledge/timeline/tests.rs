use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}
fn retain(ws: &Workspace, producer: &str, location: Location, media: &str, text: &str) -> String {
    let e = Evidence {
        source: "fixture".into(),
        location,
        method: Method::Read,
        producer: producer.into(),
        kind: EvidenceKind::Observation,
        summary: "fixture".into(),
        representation: Representation::of_bytes(media, text.as_bytes()),
        limitations: vec![],
        context: EvidenceContext::default(),
    };
    ws.artifacts()
        .unwrap()
        .retain_evidence(&e, text.as_bytes())
        .unwrap()
}
fn clock(id: &str) -> Value {
    json!({"id":id,"origin":"relative"})
}
fn event(reference: &str, id: &str, start: i64) -> Value {
    json!({"reference":reference,"clock":clock(id),"start_ms":start,"end_ms":start,"uncertainty_ms":0,"quote":"entry"})
}
#[test]
fn explicit_times_are_attributed_and_unrelated_clocks_not_sorted_together() {
    let (_w, _s, ws) = fixture();
    let a = retain(&ws, "log", Location::Whole, "text/plain", "entry");
    let out = run(
        &ws,
        &json!({"action":"reconstruct","events":[event(&a,"a",100),event(&a,"b",0)]}),
    )
    .unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["groups"].as_object().unwrap().len(), 2);
    assert!(
        data["groups"]["a"][0]["timing_basis"]
            .as_str()
            .unwrap()
            .contains("caller supplied")
    );
    assert!(
        run(
            &ws,
            &json!({"action":"reconstruct","sources":[{"reference":a}]})
        )
        .is_err()
    );
}
#[test]
fn negative_offset_retains_basis_uncertainty_and_overlap() {
    let (_w, _s, ws) = fixture();
    let a = retain(&ws, "log", Location::Whole, "text/plain", "entry");
    let out=run(&ws,&json!({"action":"reconstruct","events":[event(&a,"a",100),event(&a,"b",0)],"alignments":[{"from":"a","to":"b","offset_ms":-100,"uncertainty_ms":10,"basis":[a]}]})).unwrap();
    let d = out.data.as_ref().unwrap();
    assert_eq!(d["groups"]["b"][0]["earliest_ms"], -10);
    assert_eq!(d["overlapping_or_ambiguous_pairs"], json!([[0, 1]]));
    let r = ws
        .artifacts()
        .unwrap()
        .evidence_record(d["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(r.evidence.context.alignments[0].offset_ms, -100);
    assert!(r.evidence.context.parents.contains(&a));
}
#[test]
fn exact_audio_segments_and_video_frame_citations_are_selected_not_enclosing_interval() {
    let (_w, _s, ws) = fixture();
    let audio = retain(
        &ws,
        "dial/audio-whisper.cpp",
        Location::Time {
            start_ms: 0,
            end_ms: 1000,
        },
        "application/json",
        r#"{"segments":[{"start_ms":200,"end_ms":400,"text":"heard"}]}"#,
    );
    let frame = retain(
        &ws,
        "dial/video-ffmpeg",
        Location::Time {
            start_ms: 300,
            end_ms: 300,
        },
        "image/png",
        "fake frame bytes",
    );
    let video = retain(
        &ws,
        "dial/video-ffmpeg",
        Location::Time {
            start_ms: 0,
            end_ms: 1000,
        },
        "application/json",
        &json!({"frames":[{"requested_seek_ms":300,"citation":frame}]}).to_string(),
    );
    let out=run(&ws,&json!({"action":"reconstruct","sources":[{"reference":audio,"clock":clock("a")},{"reference":video,"clock":clock("v")}]})).unwrap();
    let d = out.data.unwrap();
    assert_eq!(d["groups"]["a"][0]["start_ms"], 200);
    assert_eq!(d["groups"]["v"][0]["reference"], frame);
    assert_eq!(d["groups"]["a"][0]["uncertainty_known"], false);
    assert!(d["groups"]["a"][0]["earliest_ms"].is_null());
    assert!(d["groups"]["v"][0]["latest_ms"].is_null());
    assert_eq!(
        d["groups"]["a"][0]["selection"]["source_kind"],
        "observation"
    );
    assert!(
        d["groups"]["v"][0]["timing_basis"]
            .as_str()
            .unwrap()
            .contains("requested seek")
    );
}
#[test]
fn missing_quotes_missing_refs_overflow_and_limits_fail_closed() {
    let (_w, _s, ws) = fixture();
    let a = retain(&ws, "log", Location::Whole, "text/plain", "entry");
    let mut e = event(&a, "a", i64::MAX);
    e["uncertainty_ms"] = json!(1);
    assert!(run(&ws, &json!({"action":"reconstruct","events":[e]})).is_err());
    let alignment = json!({"from":"a","to":"b","offset_ms":1,"uncertainty_ms":0,"basis":[a]});
    assert!(run(&ws,&json!({"action":"reconstruct","events":[event(&a,"a",i64::MAX),event(&a,"b",0)],"alignments":[alignment]})).is_err());
    let mut e = event(&a, "a", 0);
    e["quote"] = json!("absent");
    assert!(run(&ws, &json!({"action":"reconstruct","events":[e]})).is_err());
    assert!(
        run(
            &ws,
            &json!({"action":"reconstruct","events":[event("artifact://unknown","a",0)]})
        )
        .is_err()
    );
    assert!(
        run(
            &ws,
            &json!({"action":"reconstruct","events":vec![event(&a,"a",0);129]})
        )
        .is_err()
    );
}

#[test]
fn unspecified_caller_uncertainty_is_unknown_and_clock_origins_cannot_collide() {
    let (_w, _s, ws) = fixture();
    let a = retain(&ws, "log", Location::Whole, "text/plain", "entry");
    let mut e = event(&a, "a", 0);
    e.as_object_mut().unwrap().remove("uncertainty_ms");
    let out = run(
        &ws,
        &json!({"action":"reconstruct","events":[e.clone(),event(&a,"a",1000)]}),
    )
    .unwrap();
    let d = out.data.unwrap();
    assert_eq!(d["groups"]["a"][0]["uncertainty_known"], false);
    assert!(d["groups"]["a"][0]["earliest_ms"].is_null());
    assert_eq!(d["overlapping_or_ambiguous_pairs"], json!([[0, 1]]));
    let mut other = event(&a, "a", 1);
    other["clock"]["origin"] = json!("unix");
    assert!(run(&ws, &json!({"action":"reconstruct","events":[e,other]})).is_err());
}
