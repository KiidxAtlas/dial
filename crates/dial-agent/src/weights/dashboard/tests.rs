use super::*;

#[test]
fn page_serves_every_section_builder_without_innerhtml() {
    let (status, kind, body) = route(&Config::default(), "/");
    assert_eq!((status, kind), ("200 OK", HTML));
    for builder in [
        "editsSection",
        "evaluationView",
        "doseChart",
        "attributeSection",
        "rankRows",
        "headGrid",
        "routingSection",
        "experimentTimeline",
        "recentRuns",
        "scaleEditor",
        "scaleControl",
        "submitScale",
    ] {
        assert!(body.contains(builder), "page is missing {builder}");
    }
    assert!(
        !body.contains("innerHTML"),
        "page must not build markup from data"
    );
}
