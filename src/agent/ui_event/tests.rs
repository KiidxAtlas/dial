use super::*;

#[test]
fn lanes_serialize_as_snake_case_names() {
    assert_eq!(serde_json::to_value(Lane::Context).unwrap(), "context");
    assert_eq!(serde_json::to_value(Lane::Divider).unwrap(), "divider");
    assert_eq!(serde_json::to_value(Lane::Alert).unwrap(), "alert");
    assert_eq!(
        serde_json::to_value(EventKind::ContextReset.lane()).unwrap(),
        "divider"
    );
    assert_eq!(
        serde_json::to_value(EventKind::Recovery.lane()).unwrap(),
        "alert"
    );
}
