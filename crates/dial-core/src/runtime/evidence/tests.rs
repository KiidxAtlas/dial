use super::*;
use crate::runtime::tool_output::ToolOutput;
use serde_json::json;

fn evidence(location: Location) -> Evidence {
    Evidence {
        source: "workspace/app.log".into(),
        location,
        method: Method::Read,
        producer: "log-reader".into(),
        kind: EvidenceKind::Observation,
        summary: "Captured log window, not proof of the root cause".into(),
        representation: Representation::of_bytes("text/plain", b"error\n"),
        limitations: vec!["Only this window was inspected".into()],
        context: EvidenceContext::default(),
    }
}

#[test]
fn round_trip_preserves_locations_and_interpretation_kind() {
    for location in [
        Location::Whole,
        Location::Lines { start: 3, end: 9 },
        Location::Page { number: 4 },
        Location::Time {
            start_ms: 83000,
            end_ms: 83000,
        },
        Location::Region {
            x: 0,
            y: 500000,
            width: 1000000,
            height: 500000,
        },
    ] {
        let mut item = evidence(location);
        item.kind = EvidenceKind::Inference;
        item.method = Method::ModelInterpretation;
        let bundle = EvidenceBundle::new(vec![item]).unwrap();
        let output = ToolOutput::ok("model-facing text".into())
            .with_evidence(bundle.clone())
            .unwrap();
        assert_eq!(output.evidence().unwrap(), Some(bundle));
        assert!(!output.is_error);
        assert_eq!(
            output.execution,
            Default::default(),
            "evidence is not execution authority"
        );
    }
}

#[test]
fn rejects_invalid_ranges_versions_digests_and_unbounded_payloads() {
    for location in [
        Location::Lines { start: 0, end: 9 },
        Location::Lines { start: 9, end: 3 },
        Location::Page { number: 0 },
        Location::Time {
            start_ms: 2,
            end_ms: 1,
        },
        Location::Region {
            x: u32::MAX,
            y: 0,
            width: 1,
            height: 1,
        },
        Location::Region {
            x: 1,
            y: 0,
            width: 1000000,
            height: 1,
        },
        Location::Region {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
    ] {
        assert!(EvidenceBundle::new(vec![evidence(location)]).is_err());
    }
    assert!(EvidenceBundle::new(vec![]).is_err());
    assert!(EvidenceBundle::new(vec![evidence(Location::Whole); MAX_ITEMS + 1]).is_err());
    let mut item = evidence(Location::Whole);
    item.representation.sha256 = "not a digest".into();
    assert!(EvidenceBundle::new(vec![item]).is_err());
    let mut item = evidence(Location::Whole);
    item.source = " ".into();
    assert!(EvidenceBundle::new(vec![item]).is_err());
    let mut bundle = EvidenceBundle::new(vec![evidence(Location::Whole)]).unwrap();
    bundle.schema_version += 1;
    let output = ToolOutput::structured(json!({"evidence":bundle}));
    assert!(output.evidence().is_err());
}

#[test]
fn digest_identifies_representation_bytes() {
    assert_eq!(
        Representation::of_bytes("text/plain", b"abc").sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_ne!(
        Representation::of_bytes("text/plain", b"abc"),
        Representation::of_bytes("text/plain", b"abd")
    );
}

#[test]
fn attachment_preserves_payload_and_status_but_refuses_collisions() {
    let bundle = EvidenceBundle::new(vec![evidence(Location::Whole)]).unwrap();
    let mut output = ToolOutput::err("failed extraction");
    output.data = Some(json!({"artifact":{"media_type":"text/plain"}}));
    let output = output.with_evidence(bundle.clone()).unwrap();
    assert!(
        output.is_error,
        "evidence must not hide an existing failure"
    );
    assert_eq!(output.text, "failed extraction");
    assert_eq!(
        output.data.as_ref().unwrap()["artifact"]["media_type"],
        "text/plain"
    );
    assert!(output.with_evidence(bundle.clone()).is_err());
    assert!(
        ToolOutput::structured(json!([1, 2]))
            .with_evidence(bundle)
            .is_err()
    );
    assert!(
        ToolOutput::ok("legacy result".into())
            .evidence()
            .unwrap()
            .is_none()
    );
    assert!(
        ToolOutput::structured(json!({"evidence":null}))
            .evidence()
            .is_err()
    );
}

fn artifact_ref() -> String {
    format!("artifact://{}", "a".repeat(64))
}

fn coverage() -> Coverage {
    Coverage {
        extent: CoverageExtent::Complete,
        inspected: vec![Location::Whole],
        omitted: vec![],
        truncated: false,
        failures: vec![],
    }
}

fn alignment() -> ClockAlignment {
    ClockAlignment {
        from: "recording".into(),
        to: "wall".into(),
        offset_ms: -100,
        uncertainty_ms: 7,
        basis: vec![artifact_ref()],
    }
}

#[test]
fn legacy_context_is_optional_and_empty_context_is_not_serialized() {
    let old = serde_json::to_value(evidence(Location::Whole)).unwrap();
    assert!(old.get("context").is_none());
    let item: Evidence = serde_json::from_value(old).unwrap();
    assert!(item.context.is_empty());
    assert_eq!(EvidenceBundle::new(vec![item]).unwrap().schema_version, 1);
    assert!(serde_json::from_value::<EvidenceContext>(json!({"extra":1})).is_err());
    assert!(serde_json::from_value::<Coverage>(json!({"extent":"unknown","extra":1})).is_err());
    assert!(serde_json::from_value::<Clock>(json!({"id":"x","origin":"unix","extra":1})).is_err());
    assert!(
        serde_json::from_value::<SourceSnapshot>(
            json!({"sha256":"a","bytes":0,"captured_at_unix_ms":0,"extra":1})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ClockAlignment>(
            json!({"from":"x","to":"y","offset_ms":0,"uncertainty_ms":0,"extra":1})
        )
        .is_err()
    );
}

#[test]
fn context_roundtrip_retains_original_snapshot_timezone_and_assertions() {
    let mut item = evidence(Location::Whole);
    item.context = EvidenceContext {
        parents: vec![artifact_ref()],
        snapshot: Some(SourceSnapshot {
            sha256: "a".repeat(64),
            bytes: 0,
            captured_at_unix_ms: 123,
            artifact: Some(artifact_ref()),
        }),
        coverage: Some(coverage()),
        clock: Some(Clock {
            id: "recording".into(),
            origin: ClockOrigin::Relative,
            timezone: Some("America/New_York".into()),
        }),
        alignments: vec![alignment()],
    };
    item.validate().unwrap();
    let decoded: Evidence = serde_json::from_value(serde_json::to_value(&item).unwrap()).unwrap();
    assert_eq!(decoded, item);
    assert_ne!(
        item.context.snapshot.unwrap().sha256,
        item.representation.sha256
    );
}

#[test]
fn complete_coverage_rejects_gaps_and_all_regions_are_validated() {
    coverage().validate().unwrap();
    let mut invalid = vec![];
    let mut c = coverage();
    c.inspected.clear();
    invalid.push(c);
    let mut c = coverage();
    c.omitted.push(Location::Whole);
    invalid.push(c);
    let mut c = coverage();
    c.truncated = true;
    invalid.push(c);
    let mut c = coverage();
    c.failures.push("read failed".into());
    invalid.push(c);
    let mut c = coverage();
    c.inspected = vec![Location::Page { number: 0 }];
    invalid.push(c);
    for c in invalid {
        assert!(c.validate().is_err());
    }
    let mut c = coverage();
    c.extent = CoverageExtent::Partial;
    c.omitted.push(Location::Lines { start: 0, end: 3 });
    assert!(c.validate().is_err());
    c.omitted = vec![Location::Whole];
    c.truncated = true;
    c.failures.push("decoder stopped".into());
    c.validate().unwrap();
    c.extent = CoverageExtent::Unknown;
    c.validate().unwrap();
}

#[test]
fn metadata_collections_and_strings_are_bounded() {
    let mut context = EvidenceContext {
        parents: vec![artifact_ref(); MAX_ITEMS],
        ..Default::default()
    };
    context.validate().unwrap();
    context.parents.push(artifact_ref());
    assert!(context.validate().is_err());
    context.parents.clear();
    context.alignments = vec![alignment(); MAX_ITEMS + 1];
    assert!(context.validate().is_err());
    let mut a = alignment();
    a.basis = vec![artifact_ref(); MAX_ITEMS + 1];
    assert!(a.validate().is_err());
    for field in 0..3 {
        let mut c = coverage();
        c.extent = CoverageExtent::Unknown;
        match field {
            0 => c.inspected = vec![Location::Whole; MAX_ITEMS + 1],
            1 => c.omitted = vec![Location::Whole; MAX_ITEMS + 1],
            _ => c.failures = vec!["failure".into(); MAX_ITEMS + 1],
        }
        assert!(c.validate().is_err());
    }
    for text in ["".into(), "x".repeat(257), "bad\0label".into()] {
        let mut a = alignment();
        a.from = text.clone();
        assert!(a.validate().is_err());
        a = alignment();
        a.to = text.clone();
        assert!(a.validate().is_err());
        let clock = Clock {
            id: text.clone(),
            origin: ClockOrigin::Unix,
            timezone: None,
        };
        assert!(clock.validate().is_err());
        let clock = Clock {
            id: "wall".into(),
            origin: ClockOrigin::Unix,
            timezone: Some(text),
        };
        assert!(clock.validate().is_err());
    }
    let mut c = coverage();
    c.extent = CoverageExtent::Partial;
    c.failures.push("x".repeat(2049));
    assert!(c.validate().is_err());
}

#[test]
fn metadata_refs_are_strict_and_snapshot_retains_original_identity() {
    for reference in [
        "http://example.org".into(),
        "artifact://abc".into(),
        format!("artifact://{}", "A".repeat(64)),
        format!("{}#lines", artifact_ref()),
    ] {
        let context = EvidenceContext {
            parents: vec![reference.clone()],
            ..Default::default()
        };
        assert!(context.validate().is_err());
        let mut a = alignment();
        a.basis = vec![reference.clone()];
        assert!(a.validate().is_err());
        let snapshot = SourceSnapshot {
            sha256: "a".repeat(64),
            bytes: 1,
            captured_at_unix_ms: 0,
            artifact: Some(reference),
        };
        assert!(snapshot.validate().is_err());
    }
    let mut snapshot = SourceSnapshot {
        sha256: "b".repeat(64),
        bytes: 1,
        captured_at_unix_ms: 0,
        artifact: Some(artifact_ref()),
    };
    assert!(snapshot.validate().is_err());
    snapshot.artifact = None;
    snapshot.validate().unwrap();
    snapshot.sha256 = "invalid".into();
    assert!(snapshot.validate().is_err());
    let mut item = evidence(Location::Whole);
    item.context.parents.push("bad".into());
    assert!(EvidenceBundle::new(vec![item]).is_err());
}

#[test]
fn clock_mapping_is_signed_checked_and_preserves_uncertainty() {
    let mut a = alignment();
    assert_eq!(
        a.aligned_time(50).unwrap(),
        AlignedTime {
            time_ms: -50,
            uncertainty_ms: 7
        }
    );
    assert!(a.aligned_time(i64::MIN).is_err());
    a.offset_ms = 1;
    assert!(a.aligned_time(i64::MAX).is_err());
    assert_eq!(a.aligned_time(-1).unwrap().time_ms, 0);
    a.to = a.from.clone();
    assert!(a.validate().is_err());
    assert!(a.aligned_time(0).is_err());
}
