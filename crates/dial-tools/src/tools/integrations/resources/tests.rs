use super::*;
#[test]
fn routing_preserves_local_selectors_and_refuses_unknown_authority() {
    assert_eq!(scheme("src/main.rs:5-10").unwrap(), Scheme::File);
    assert!(scheme("http://example.com").is_err());
    assert!(scheme("artifact://../x").is_err());
    assert_eq!(
        artifact_id(&format!("artifact://{}", "A".repeat(64))).unwrap(),
        "a".repeat(64)
    );
}

#[test]
fn artifact_selectors_choose_lines_info_annotations_or_bytes() {
    let id = "a".repeat(64);
    let uri = format!("artifact://{id}");
    let view =
        |suffix: &str| artifact_target(&format!("{uri}{suffix}")).map(|(u, v)| (u == uri, v));
    let lines = |start, count| Ok((true, ArtifactView::Lines { start, count }));
    assert_eq!(view(""), lines(1, None));
    assert_eq!(view(":50-60"), lines(50, Some(11)));
    assert_eq!(view(":50+20"), lines(50, Some(20)));
    assert_eq!(view(":50-"), lines(50, None));
    assert_eq!(view(":7"), lines(7, Some(1)));
    assert_eq!(view(":info"), Ok((true, ArtifactView::Info)));
    assert_eq!(view(":annotations"), Ok((true, ArtifactView::Annotations)));
    assert_eq!(
        view(":bytes=10+32"),
        Ok((
            true,
            ArtifactView::Bytes {
                offset: 10,
                count: 32
            }
        ))
    );
    assert_eq!(
        view(":bytes=0"),
        Ok((
            true,
            ArtifactView::Bytes {
                offset: 0,
                count: u64::MAX
            }
        ))
    );
    for bad in [":0", ":5+0", ":9-3", ":nope", ":bytes=x"] {
        assert!(view(bad).is_err(), "{bad}");
    }
    assert_eq!(scheme(&format!("{uri}:info")).unwrap(), Scheme::Artifact);
}
