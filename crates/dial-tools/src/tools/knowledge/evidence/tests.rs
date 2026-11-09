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

#[test]
fn merged_schema_keeps_native_properties_and_offers_every_merged_action() {
    let schema = super::schema();
    let native = super::native_schema();
    let properties = &schema["function"]["parameters"]["properties"];
    for (name, property) in native["function"]["parameters"]["properties"]
        .as_object()
        .unwrap()
    {
        if name != "action" {
            assert_eq!(&properties[name], property, "{name}");
        }
    }
    let actions: Vec<&str> = properties["action"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    for &(_, _, merged) in MERGED_ACTIONS {
        assert_eq!(
            actions.iter().filter(|a| **a == merged).count(),
            1,
            "{merged}"
        );
    }
    // Two owners bound `references` differently; the merged shape keeps the item type.
    assert_eq!(
        properties["references"],
        json!({"type":"array","items":{"type":"string"}})
    );
    // `assessment` unites the investigate and reality judgements.
    let assessments = properties["assessment"]["enum"].as_array().unwrap();
    assert!(assessments.contains(&json!("supports")));
    assert!(assessments.contains(&json!("mismatch")));
    crate::runtime::schema::check_schema(&schema["function"]["parameters"]).unwrap();
}

#[test]
fn merged_actions_round_trip_to_the_retired_tool_arguments() {
    for &(tool, recorded, merged) in MERGED_ACTIONS {
        let (legacy_tool, legacy) =
            legacy_call(&json!({"action":merged,"references":["artifact://x"]})).unwrap();
        assert_eq!(legacy_tool, tool);
        assert_eq!(legacy.get("action").and_then(Value::as_str), recorded);
        assert_eq!(merged_action(tool, &legacy), Some(merged));
    }
    assert!(legacy_call(&json!({"action":"capture"})).is_none());
    assert_eq!(
        merged_action("workbench", &json!({"action":"capture"})),
        None
    );
    assert_eq!(
        merged_action("evidence_search", &json!({"action":"search"})),
        None
    );
}

#[test]
fn merged_actions_run_their_owner_with_its_own_bounds() {
    let (work, _state, ws) = fixture();
    fs::write(work.path().join("a.txt"), "timeout after five seconds\n").unwrap();
    fs::write(work.path().join("b.txt"), "timeout after nine seconds\n").unwrap();
    let a = capture(&ws, "a.txt");
    let b = capture(&ws, "b.txt");
    let cancel = AtomicBool::new(false);
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let run = |args: Value| dispatch(&ctx, &args, &cancel);

    let audit = run(json!({"action":"audit","references":[a]})).unwrap();
    assert!(!audit.is_error, "{}", audit.text);
    let search = run(json!({"action":"search","query":"timeout","references":[a, b]})).unwrap();
    assert!(search.text.contains("five"), "{}", search.text);
    let diff = run(json!({"action":"diff","reference":a,"other":b})).unwrap();
    assert!(diff.text.contains("nine"), "{}", diff.text);
    // The native `compare` still compares exact bytes rather than diffing text.
    let compare = run(json!({"action":"compare","reference":a,"other":b})).unwrap();
    assert!(!compare.is_error, "{}", compare.text);

    // workbench accepted at most 16 references; the widened schema does not lift that.
    let many: Vec<String> = (0..17).map(|_| a.clone()).collect();
    let error = run(json!({"action":"audit","references":many})).unwrap_err();
    assert!(format!("{error:#}").contains("evidence audit"), "{error:#}");
    // A property from another merged tool is refused for this action.
    assert!(run(json!({"action":"audit","references":[a],"query":"x"})).is_err());
    // Semantic ranking still needs the agent's model execution.
    assert!(
        run(json!({"action":"search","query":"x","references":[a],"ranking":"semantic"})).is_err()
    );
}

#[test]
fn budget_actions_keep_perception_approval_confirmation_and_exclusivity() {
    use crate::tools::integrations::ext::Concurrency;
    let tool = super::tools(&Config::default()).remove(0);
    let limits = json!({"images":1,"pages":0,"frames":0,"audio_ms":0,"operations":2});
    let open = json!({"action":"budget_open","question":"Which frame?","limits":limits});
    let status = json!({"action":"budget_status"});
    for (args, gated) in [
        (open.clone(), true),
        (
            json!({"action":"budget_step","tool":"visual","request":{},"reason":"r"}),
            true,
        ),
        (json!({"action":"budget_close","reason":"done"}), true),
        (status.clone(), false),
        (json!({"action":"capture","path":"a.txt"}), false),
        (
            json!({"action":"audit","references":["artifact://x"]}),
            false,
        ),
    ] {
        assert_eq!(tool.needs_approval(&args), gated, "{args}");
        let expected = if gated {
            Concurrency::Exclusive
        } else {
            Concurrency::Shared
        };
        assert_eq!(tool.concurrency(&args), expected, "{args}");
    }

    let (_work, _state, ws) = fixture();
    let config = Config::default();
    let mut ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    // Opening still needs a real human confirmation, not just approval of the call.
    let refused = tool.call(&ctx, &open);
    assert!(refused.is_error, "{}", refused.text);
    ctx.confirm = Some(&|_| true);
    let opened = tool.call(&ctx, &open);
    assert!(!opened.is_error, "{}", opened.text);
    let counted = tool.call(&ctx, &status);
    assert_eq!(counted.data.unwrap()["budget"]["limits"]["operations"], 2);
    // perception's own bounds apply: at most 128 operations.
    let too_many = tool.call(
        &ctx,
        &json!({"action":"budget_open","question":"q","limits":{"images":0,"pages":0,"frames":0,"audio_ms":0,"operations":129}}),
    );
    assert!(too_many.is_error);
    let closed = tool.call(&ctx, &json!({"action":"budget_close","reason":"done"}));
    assert!(!closed.is_error, "{}", closed.text);
}
