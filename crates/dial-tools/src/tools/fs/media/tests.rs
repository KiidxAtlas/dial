use super::*;

fn probe(duration: Option<f64>, fps: Option<f64>) -> Probe {
    Probe {
        duration,
        fps,
        width: 1920,
        height: 1080,
        video_codec: "h264".to_string(),
        audio_codec: None,
    }
}

#[test]
fn split_keeps_clock_colons_in_video_selectors() {
    assert_eq!(split("clip.mp4:01:23"), ("clip.mp4", "01:23"));
    assert_eq!(split("a.pdf:page=2:img"), ("a.pdf", "page=2:img"));
    assert_eq!(split("plain"), ("plain", ""));
}

#[test]
fn pdf_page_selector_requires_image_suffix_and_positive_page() {
    assert_eq!(pdf_page("page=3:img"), Some(3));
    assert_eq!(pdf_page("page=0:img"), None);
    assert_eq!(pdf_page("page=3"), None);
    assert_eq!(pdf_page("page=10000:img"), Some(10000));
    assert_eq!(pdf_page("page=10001:img"), None);
}

#[test]
fn unit_durations_sum_hours_minutes_and_seconds() {
    assert_eq!(parse_units("1h5m42s"), Some(3942.0));
    assert_eq!(parse_units("90s"), Some(90.0));
    assert_eq!(parse_units("12.5s"), Some(12.5));
    assert_eq!(parse_units("5"), None);
    assert_eq!(parse_units("1h5"), None);
}

#[test]
fn clock_selectors_accept_mm_ss_and_hh_mm_ss_only() {
    assert_eq!(parse_clock("01:23"), Some(83.0));
    assert_eq!(parse_clock("1:02:03"), Some(3723.0));
    assert_eq!(parse_clock("1:2:3:4"), None);
}

#[test]
fn frame_numbers_map_through_frame_rate_and_respect_duration() {
    let video = probe(Some(10.0), Some(30.0));
    assert_eq!(locate("30", &video).unwrap(), 1.0);
    assert!(locate("300", &video).is_err());
}

#[test]
fn positions_beyond_duration_are_rejected() {
    let video = probe(Some(10.0), Some(30.0));
    assert!(locate("11s", &video).is_err());
    assert!(locate("bogus", &video).is_err());
}

#[test]
fn frame_rate_rationals_parse_and_reject_zero_denominators() {
    assert!((parse_rate("30000/1001").unwrap() - 29.97).abs() < 0.01);
    assert_eq!(parse_rate("0/0"), None);
}

#[test]
fn svg_rasterizes_to_a_png_at_the_minimum_legible_edge() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="red"/></svg>"#;
    let png = rasterize_svg(svg).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(images::dimensions(&png).unwrap(), (512, 1024));
}

#[test]
fn invalid_svg_is_rejected() {
    assert!(rasterize_svg(b"not an svg").is_err());
}

#[test]
fn svg_read_emits_evidence_for_the_exact_returned_png() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("drawing.svg");
    fs::write(&path, br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="red"/></svg>"#).unwrap();
    let output = read(
        &ws,
        &json!({"path":"drawing.svg:img"}),
        &AtomicBool::new(false),
        None,
    )
    .unwrap()
    .unwrap();
    let bundle = output.evidence().unwrap().unwrap();
    let item = &bundle.items[0];
    assert_eq!(item.source, "drawing.svg");
    assert_eq!(item.location, Location::Whole);
    assert_eq!(item.method, Method::Render);
    assert_eq!(item.kind, EvidenceKind::Observation);
    let artifact = &output.data.as_ref().unwrap()["artifact"];
    let png = STANDARD.decode(artifact["data"].as_str().unwrap()).unwrap();
    assert_eq!(
        item.representation,
        Representation::of_bytes("image/png", &png)
    );
    assert!(!item.limitations.is_empty());
    assert!(!output.is_error);
}

#[test]
fn derived_page_and_video_evidence_keep_location_without_claiming_verification() {
    let png = rasterize_svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#)
        .unwrap();
    for location in [
        Location::Page { number: 2 },
        Location::Time {
            start_ms: 12345,
            end_ms: 12345,
        },
    ] {
        let output = derived(
            "Extracted representation".into(),
            &png,
            Origin {
                source: "source",
                location: location.clone(),
                method: Method::Sample,
                producer: "test-extractor",
                limitations: &["Test fixture; no external converter was run"],
            },
        )
        .unwrap();
        let bundle = output.evidence().unwrap().unwrap();
        assert_eq!(bundle.items[0].location, location);
        assert_eq!(output.execution, Default::default());
    }
    assert!(
        derived(
            "invalid".into(),
            b"not an image",
            Origin {
                source: "source",
                location: Location::Whole,
                method: Method::Render,
                producer: "test-extractor",
                limitations: &[],
            }
        )
        .is_err()
    );
}

#[test]
fn svg_read_retains_reopenable_citation_for_returned_png_not_original_source() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(state.path()).unwrap());
    let path = dir.path().join("drawing.svg");
    fs::write(&path, br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="red"/></svg>"#).unwrap();
    let output = read(
        &ws,
        &json!({"path":"drawing.svg:img"}),
        &AtomicBool::new(false),
        None,
    )
    .unwrap()
    .unwrap();
    let data = output.data.as_ref().unwrap();
    let png = STANDARD
        .decode(data["artifact"]["data"].as_str().unwrap())
        .unwrap();
    let citation = data["evidence_references"][0].as_str().unwrap();
    assert!(output.text.contains(citation));
    fs::remove_file(&path).unwrap();
    let reopened = crate::storage::artifact::ArtifactStore::new(state.path()).unwrap();
    let record = reopened.evidence_record(citation).unwrap();
    assert_eq!(
        record.evidence,
        output.evidence().unwrap().unwrap().items[0]
    );
    assert_eq!(
        record.evidence.representation,
        Representation::of_bytes("image/png", &png)
    );
    assert_eq!(reopened.evidence_bytes(&record, png.len()).unwrap(), png);
    assert!(record.evidence.context.snapshot.is_none());
    assert_eq!(
        record.evidence.context.coverage.unwrap().extent,
        CoverageExtent::Partial
    );
}

#[test]
fn external_reads_reject_before_open_or_converter_lookup_without_both_grants() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let config = crate::config::types::Config::default();
    let cancel = AtomicBool::new(false);
    for path in ["missing.mp4", "missing.heic", "missing.pdf:page=1:img"] {
        let args = json!({"path": path});
        let output = crate::tools::read_tool_streaming(&ws, &args, &cancel, 4096);
        assert!(
            output.is_error && output.text.contains("authorization"),
            "{}",
            output.text
        );
        let ctx = ToolContext {
            workspace: &ws,
            config: &config,
            confirm: None,
            mutation_authorizer: None,
        };
        let output = crate::tools::read_with_context(&ctx, &args, &cancel, 4096);
        assert!(
            output.is_error && output.text.contains("confirmation"),
            "{}",
            output.text
        );
        let ctx = ToolContext {
            confirm: Some(&|_| true),
            ..ctx
        };
        let output = crate::tools::read_with_context(&ctx, &args, &cancel, 4096);
        assert!(
            output.is_error && output.text.contains("execution-controller"),
            "{}",
            output.text
        );
    }
}

#[test]
fn handles_route_before_media_detection_and_plain_reads_stay_available() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("a.mp4"), b"not a video").unwrap();
    let handle = ws.file_handle("a.mp4").unwrap();
    let cancel = AtomicBool::new(false);
    let output = crate::tools::read_tool_streaming(&ws, &json!({"path":handle}), &cancel, 4096);
    assert!(
        output.is_error && output.text.contains("authorization"),
        "{}",
        output.text
    );
    fs::write(dir.path().join("a.txt"), b"ordinary text\n").unwrap();
    let output = crate::tools::read_tool_streaming(&ws, &json!({"path":"a.txt"}), &cancel, 4096);
    assert!(
        !output.is_error && output.text.contains("ordinary text"),
        "{}",
        output.text
    );
    let output = crate::tools::read_tool_streaming(&ws, &json!({"paths":[handle]}), &cancel, 4096);
    assert!(output.text.contains("single path"), "{}", output.text);
}

#[test]
fn converter_output_read_is_bounded_even_for_sparse_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("output.png");
    fs::write(&path, b"small output").unwrap();
    assert_eq!(bounded_output(&path).unwrap(), b"small output");
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(images::MAX_IMAGE_SOURCE_BYTES + 1)
        .unwrap();
    assert!(
        bounded_output(&path)
            .unwrap_err()
            .to_string()
            .contains("64 MiB")
    );
}

#[test]
fn authorized_sources_still_enforce_confinement_and_size_before_conversion() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let config = crate::config::types::Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|tool, paths, outside| {
            assert_eq!(tool, "read");
            assert!(paths.is_empty() && outside);
            Ok(())
        }),
    };
    let path = dir.path().join("oversized.mp4");
    fs::File::create(path)
        .unwrap()
        .set_len(source::MAX_SOURCE + 1)
        .unwrap();
    let output = crate::tools::read_with_context(
        &ctx,
        &json!({"path":"oversized.mp4"}),
        &AtomicBool::new(false),
        4096,
    );
    assert!(
        output.is_error && output.text.contains("64 MiB"),
        "{}",
        output.text
    );
    let outside = tempfile::tempdir().unwrap();
    let output = crate::tools::read_with_context(
        &ctx,
        &json!({"path":outside.path().join("a.mp4")}),
        &AtomicBool::new(false),
        4096,
    );
    assert!(
        output.is_error && !output.text.contains("ffprobe"),
        "{}",
        output.text
    );
}

#[test]
fn captured_source_revision_is_attached_without_replacing_pixels() {
    let png = rasterize_svg(br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#)
        .unwrap();
    let output = derived(
        "Rendered page".into(),
        &png,
        Origin {
            source: "source.pdf",
            location: Location::Page { number: 1 },
            method: Method::Render,
            producer: "test",
            limitations: &[],
        },
    )
    .unwrap();
    let artifact = output.data.as_ref().unwrap()["artifact"].clone();
    let captured = source::snapshot(b"exact captured source bytes").unwrap();
    let output = with_snapshot(output, captured.clone()).unwrap();
    assert_eq!(output.data.as_ref().unwrap()["artifact"], artifact);
    let evidence = output.evidence().unwrap().unwrap();
    assert_eq!(evidence.items[0].context.snapshot, Some(captured));
    assert_eq!(evidence.items[0].location, Location::Page { number: 1 });
}
