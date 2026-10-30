use super::*;
use crate::tools::testkit;
use std::fs;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (work, state, ws)
}

fn call(ws: &Workspace, args: Value) -> Result<ToolOutput> {
    super::run(ws, serde_json::from_value(args)?)
}

fn citation(out: &ToolOutput) -> String {
    out.data.as_ref().unwrap()["evidence_references"][0]
        .as_str()
        .unwrap()
        .to_owned()
}

fn capture(ws: &Workspace, path: &str) -> String {
    citation(&call(ws, json!({"action":"capture", "path":path})).unwrap())
}

fn stored(ws: &Workspace, reference: &str) -> StoredEvidence {
    ws.artifacts().unwrap().evidence_record(reference).unwrap()
}

fn missing_reference() -> String {
    format!("artifact://{}", "0".repeat(64))
}

#[test]
fn capture_preserves_exact_bytes_and_reopens_durable_citations_with_original_context() {
    let (work, state, ws) = fixture();
    let original = "first\r\nsecond café\r\nthird\nlast";
    fs::write(work.path().join("source.txt"), original).unwrap();
    let out = call(&ws, json!({"action":"capture", "path":"source.txt", "start":2, "count":1, "retain_source":true})).unwrap();
    let reference = citation(&out);
    let record = stored(&ws, &reference);
    let store = ws.artifacts().unwrap();
    assert_eq!(
        store.evidence_bytes(&record, 1024).unwrap(),
        "second café\r\n".as_bytes()
    );
    assert_eq!(
        record.evidence.location,
        Location::Lines { start: 2, end: 2 }
    );
    assert_eq!(
        record.evidence.context.coverage.as_ref().unwrap().extent,
        CoverageExtent::Partial
    );
    assert_eq!(
        record.evidence.context.snapshot.as_ref().unwrap().bytes,
        original.len() as u64
    );
    assert!(
        record
            .evidence
            .context
            .snapshot
            .as_ref()
            .unwrap()
            .artifact
            .is_some()
    );

    fs::write(work.path().join("source.txt"), "replacement\n").unwrap();
    let reopened = Workspace::new(work.path())
        .unwrap()
        .with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let inspected = call(
        &reopened,
        json!({"action":"inspect", "reference":reference}),
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(inspected["text"], "second café\r\n");
    assert_eq!(inspected["representation_lines_total"], 1);
    let context = call(
        &reopened,
        json!({"action":"context", "reference":reference, "start":3, "count":2}),
    )
    .unwrap();
    let context_record = stored(&reopened, &citation(&context));
    assert_eq!(
        reopened
            .artifacts()
            .unwrap()
            .evidence_bytes(&context_record, 1024)
            .unwrap(),
        b"third\nlast"
    );
    assert_eq!(
        context_record.evidence.location,
        Location::Lines { start: 3, end: 4 }
    );
    assert_eq!(context_record.evidence.context.parents, vec![reference]);
}

#[test]
fn context_without_original_cannot_read_outside_retained_representation() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("source.txt"), "first\nsecond\nthird\n").unwrap();
    let out = call(
        &ws,
        json!({"action":"capture", "path":"source.txt", "start":2, "count":1}),
    )
    .unwrap();
    let reference = citation(&out);
    assert!(
        stored(&ws, &reference)
            .evidence
            .context
            .snapshot
            .unwrap()
            .artifact
            .is_none()
    );
    let context = call(
        &ws,
        json!({"action":"context", "reference":reference, "start":1, "count":100}),
    )
    .unwrap();
    let record = stored(&ws, &citation(&context));
    assert_eq!(record.evidence.source, reference);
    assert_eq!(
        record.evidence.location,
        Location::Lines { start: 1, end: 1 }
    );
    assert!(record.evidence.context.snapshot.is_none());
    assert_eq!(
        ws.artifacts()
            .unwrap()
            .evidence_bytes(&record, 1024)
            .unwrap(),
        b"second\n"
    );
    assert_eq!(
        record.evidence.context.coverage.unwrap().extent,
        CoverageExtent::Partial
    );
    assert!(
        call(
            &ws,
            json!({"action":"context", "reference":reference, "start":2})
        )
        .is_err()
    );
}

#[test]
fn comparison_and_freshness_distinguish_selected_bytes_from_original_snapshot() {
    let (work, _state, ws) = fixture();
    let path = work.path().join("source.txt");
    fs::write(&path, "same\nold\n").unwrap();
    let first = citation(
        &call(
            &ws,
            json!({"action":"capture", "path":"source.txt", "count":1}),
        )
        .unwrap(),
    );
    let fresh = call(&ws, json!({"action":"freshness", "reference":first}))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(fresh["unchanged_at_check"], true);
    fs::write(&path, "same\nnew\n").unwrap();
    let second = citation(
        &call(
            &ws,
            json!({"action":"capture", "path":"source.txt", "count":1}),
        )
        .unwrap(),
    );
    let compared = call(
        &ws,
        json!({"action":"compare", "reference":first, "other":second}),
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(compared["identical_representation_bytes"], true);
    assert_eq!(compared["same_source_snapshot"], false);
    assert_eq!(compared["same_source_locator"], true);
    assert!(compared["first_different_byte_offset"].is_null());
    let fresh = call(&ws, json!({"action":"freshness", "reference":first}))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(fresh["unchanged_at_check"], false);
    fs::write(&path, "sand\nnew\n").unwrap();
    let third = capture(&ws, "source.txt");
    let compared = call(
        &ws,
        json!({"action":"compare", "reference":first, "other":third}),
    )
    .unwrap()
    .data
    .unwrap();
    assert_eq!(compared["identical_representation_bytes"], false);
    assert_eq!(compared["first_different_byte_offset"], 2);
}

#[test]
fn capture_and_freshness_reject_outside_workspace_sources() {
    let (work, _state, ws) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().join("outside.txt");
    fs::write(&outside_path, "outside\n").unwrap();
    assert!(call(&ws, json!({"action":"capture", "path":outside_path})).is_err());
    fs::write(work.path().join("inside.txt"), "inside\n").unwrap();
    let reference = capture(&ws, "inside.txt");
    let store = ws.artifacts().unwrap();
    let mut record = stored(&ws, &reference);
    record.evidence.source = outside_path.to_str().unwrap().into();
    let bytes = store.evidence_bytes(&record, 1024).unwrap();
    let forged_locator = store.retain_evidence(&record.evidence, &bytes).unwrap();
    assert!(
        call(
            &ws,
            json!({"action":"freshness", "reference":forged_locator})
        )
        .is_err()
    );

    #[cfg(unix)]
    {
        fs::remove_file(work.path().join("inside.txt")).unwrap();
        std::os::unix::fs::symlink(&outside_path, work.path().join("inside.txt")).unwrap();
        assert!(call(&ws, json!({"action":"freshness", "reference":reference})).is_err());
        assert!(call(&ws, json!({"action":"capture", "path":"inside.txt"})).is_err());
    }
}

#[test]
fn derive_retains_inference_lineage_and_rejects_missing_parents() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("a.txt"), "observation A\n").unwrap();
    fs::write(work.path().join("b.txt"), "observation B\n").unwrap();
    let a = capture(&ws, "a.txt");
    let b = capture(&ws, "b.txt");
    let out = call(&ws, json!({"action":"derive", "reference":a, "parents":[b], "summary":"Possible explanation, not established fact"})).unwrap();
    let record = stored(&ws, &citation(&out));
    assert_eq!(record.evidence.kind, EvidenceKind::Inference);
    assert_eq!(record.evidence.method, Method::ModelInterpretation);
    assert_eq!(record.evidence.context.parents, vec![b, a.clone()]);
    let error = call(&ws, json!({"action":"derive", "reference":a, "parents":[missing_reference()], "summary":"Unattributed inference"})).unwrap_err();
    assert!(format!("{error:#}").contains("parent"));
}

#[test]
fn alignment_applies_signed_offset_preserves_uncertainty_and_requires_retained_basis() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("clock.txt"), "sync pulse\n").unwrap();
    let reference = capture(&ws, "clock.txt");
    let alignment = json!({"from":"camera", "to":"log", "offset_ms":-250, "uncertainty_ms":12, "basis":[reference]});
    let out = call(
        &ws,
        json!({"action":"align", "reference":reference, "alignment":alignment, "time_ms":100}),
    )
    .unwrap();
    let record = stored(&ws, &citation(&out));
    let bytes = ws
        .artifacts()
        .unwrap()
        .evidence_bytes(&record, 4096)
        .unwrap();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        result["aligned_time"],
        json!({"time_ms":-150,"uncertainty_ms":12})
    );
    assert_eq!(record.evidence.kind, EvidenceKind::Inference);
    assert_eq!(record.evidence.method, Method::Calculation);
    assert_eq!(record.evidence.context.parents, vec![reference.clone()]);
    assert_eq!(record.evidence.context.alignments[0].offset_ms, -250);
    for basis in [json!([]), json!([missing_reference()])] {
        let mut invalid = alignment.clone();
        invalid["basis"] = basis;
        assert!(
            call(
                &ws,
                json!({"action":"align", "reference":reference, "alignment":invalid, "time_ms":100})
            )
            .is_err()
        );
    }
    assert!(call(&ws, json!({"action":"align", "reference":reference, "alignment":alignment, "time_ms":i64::MIN})).is_err());
}

#[test]
fn crop_retains_exact_pixel_dimensions_and_parent_reference() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("source.txt"), "image provenance\n").unwrap();
    let seed = capture(&ws, "source.txt");
    let pixels = image::RgbaImage::from_fn(8, 6, |x, y| image::Rgba([x as u8, y as u8, 128, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels.clone())
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let png = encoded.into_inner();
    let mut item = stored(&ws, &seed).evidence;
    item.representation = Representation::of_bytes("image/png", &png);
    let reference = ws
        .artifacts()
        .unwrap()
        .retain_evidence(&item, &png)
        .unwrap();
    let out = call(&ws, json!({"action":"crop", "reference":reference, "region":{"x":250000,"y":0,"width":500000,"height":500000}})).unwrap();
    let cropped_ref = citation(&out);
    let record = stored(&ws, &cropped_ref);
    let bytes = ws
        .artifacts()
        .unwrap()
        .evidence_bytes(&record, 4096)
        .unwrap();
    let cropped = image::load_from_memory(&bytes).unwrap().to_rgba8();
    assert_eq!(cropped.dimensions(), (4, 3));
    assert_eq!(cropped.get_pixel(0, 0), pixels.get_pixel(2, 0));
    assert_eq!(cropped.get_pixel(3, 2), pixels.get_pixel(5, 2));
    assert_eq!(record.evidence.context.parents, vec![reference.clone()]);
    let inspected = call(&ws, json!({"action":"inspect", "reference":cropped_ref}))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(
        STANDARD
            .decode(inspected["artifact"]["data"].as_str().unwrap())
            .unwrap(),
        bytes
    );
    assert!(call(&ws, json!({"action":"crop", "reference":reference, "region":{"x":900000,"y":0,"width":500000,"height":1}})).is_err());
}

#[test]
fn malformed_ranges_oversized_sources_and_unknown_fields_are_rejected() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("source.txt"), "one\ntwo\n").unwrap();
    for args in [
        json!({"action":"capture", "path":"source.txt", "start":0}),
        json!({"action":"capture", "path":"source.txt", "start":3}),
        json!({"action":"capture", "path":"source.txt", "count":0}),
        json!({"action":"capture", "path":"source.txt", "count":501}),
        json!({"action":"capture", "path":"source.txt", "unexpected":true}),
        json!({"action":"crop", "region":{"x":0,"y":0,"width":1,"height":1,"unexpected":true}}),
    ] {
        assert!(call(&ws, args.clone()).is_err(), "accepted {args}");
    }
    fs::write(work.path().join("long.txt"), "x".repeat(MAX_TEXT + 1)).unwrap();
    assert!(call(&ws, json!({"action":"capture", "path":"long.txt"})).is_err());
    fs::write(work.path().join("binary.txt"), [0xff, 0xfe]).unwrap();
    assert!(call(&ws, json!({"action":"capture", "path":"binary.txt"})).is_err());
    let oversized = fs::File::create(work.path().join("large.txt")).unwrap();
    oversized.set_len(source::MAX_SOURCE + 1).unwrap();
    let error = call(&ws, json!({"action":"capture", "path":"large.txt"})).unwrap_err();
    assert!(error.to_string().contains("64 MiB"));
}

#[test]
fn missing_retained_payload_never_falls_back_to_mutable_source() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("source.txt"), "captured\n").unwrap();
    let reference = capture(&ws, "source.txt");
    let record = stored(&ws, &reference);
    let image_reference = ws
        .artifacts()
        .unwrap()
        .image_reference(&record.representation_uri)
        .unwrap();
    let blob = std::path::Path::new(image_reference["root"].as_str().unwrap())
        .join(image_reference["path"].as_str().unwrap());
    fs::remove_file(blob).unwrap();
    fs::write(work.path().join("source.txt"), "new mutable contents\n").unwrap();
    for action in ["inspect", "context"] {
        let error = call(&ws, json!({"action":action, "reference":reference})).unwrap_err();
        assert!(format!("{error:#}").contains("unavailable"));
    }
    assert!(
        call(
            &ws,
            json!({"action":"inspect", "reference":missing_reference()})
        )
        .is_err()
    );
}

#[test]
fn binary_snapshot_hashes_original_without_claiming_interpretation_and_checks_freshness() {
    let (work, _state, ws) = fixture();
    let bytes = b"\x00\xff\xfeoriginal\n";
    fs::write(work.path().join("binary.dat"), bytes).unwrap();
    let out = call(
        &ws,
        json!({"action":"snapshot", "path":"binary.dat", "retain_source":true}),
    )
    .unwrap();
    let reference = citation(&out);
    let record = stored(&ws, &reference);
    assert_eq!(record.evidence.location, Location::Whole);
    assert_eq!(
        record.evidence.context.coverage.as_ref().unwrap().extent,
        CoverageExtent::Unknown
    );
    assert_eq!(
        record.evidence.representation.media_type,
        "application/json"
    );
    let snapshot = record.evidence.context.snapshot.as_ref().unwrap();
    assert_eq!(snapshot.bytes, bytes.len() as u64);
    assert!(snapshot.artifact.is_some());
    let representation = ws
        .artifacts()
        .unwrap()
        .evidence_bytes(&record, 4096)
        .unwrap();
    let identity: Value = serde_json::from_slice(&representation).unwrap();
    assert_eq!(identity["sha256"], snapshot.sha256);
    assert_eq!(identity["bytes"], bytes.len());
    let fresh = call(&ws, json!({"action":"freshness", "reference":reference}))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(fresh["unchanged_at_check"], true);
    fs::write(work.path().join("binary.dat"), b"\x00\xffchanged").unwrap();
    let changed = call(&ws, json!({"action":"freshness", "reference":reference}))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(changed["unchanged_at_check"], false);
}

#[test]
fn capture_and_freshness_honor_both_sandbox_deny_read_policies() {
    use crate::tools::exec::sandbox::Policy;
    let (work, _state, ws) = fixture();
    let path = work.path().join("private.txt");
    fs::write(&path, "not for this read policy\n").unwrap();
    let reference = capture(&ws, "private.txt");
    for policy in [
        Policy::Dev {
            deny_read: vec![path.clone()],
            extra_write: vec![],
            allow_network: false,
        },
        Policy::Strict {
            deny_read: vec![path],
            extra_write: vec![],
            allow_network: false,
        },
    ] {
        let restricted = ws.clone().with_policy(policy);
        assert!(
            call(
                &restricted,
                json!({"action":"capture","path":"private.txt"})
            )
            .is_err()
        );
        assert!(
            call(
                &restricted,
                json!({"action":"snapshot","path":"private.txt"})
            )
            .is_err()
        );
        assert!(
            call(
                &restricted,
                json!({"action":"freshness","reference":reference})
            )
            .is_err()
        );
        // Retained bytes do not require new source access, but never authorize freshness reads.
        assert!(
            call(
                &restricted,
                json!({"action":"inspect","reference":reference})
            )
            .is_ok()
        );
    }
}
