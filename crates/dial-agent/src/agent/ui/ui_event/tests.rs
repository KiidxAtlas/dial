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

#[test]
fn work_wait_is_optional_typed_metadata_not_a_success_verdict() {
    let plain = serde_json::to_value(UiEvent::new(EventKind::Model, "model changed")).unwrap();
    assert!(plain.get("background_wait").is_none());
    let started =
        UiEvent::new(EventKind::Wait, "waiting").with_background_wait(BackgroundWait::Started {
            jobs: vec!["check-1".into()],
            workers: Vec::new(),
            questions: Vec::new(),
        });
    let encoded = serde_json::to_value(started).unwrap();
    assert_eq!(encoded["background_wait"]["state"], "started");
    assert_eq!(encoded["background_wait"]["jobs"][0], "check-1");
    assert_eq!(EventKind::Wait.lane(), Lane::Context);
    let ended = UiEvent::new(EventKind::Wait, "wait ended, inspect results").with_background_wait(
        BackgroundWait::Ended {
            reason: "interrupted".into(),
            elapsed_ms: 5,
        },
    );
    let encoded = serde_json::to_value(ended).unwrap();
    assert_eq!(encoded["background_wait"]["reason"], "interrupted");
    assert!(encoded["background_wait"].get("success").is_none());
}

#[test]
fn detail_is_optional_and_tagged_by_type() {
    let plain = serde_json::to_value(UiEvent::new(EventKind::Model, "model changed")).unwrap();
    assert!(plain.get("detail").is_none());
    let switch = UiEvent::new(EventKind::Model, "fallback").with_detail(EventDetail::ModelSwitch {
        from: Some("anthropic/opus".into()),
        to: "openai/gpt".into(),
        reason: SwitchReason::Fallback,
    });
    let encoded = serde_json::to_value(switch).unwrap();
    assert_eq!(encoded["detail"]["type"], "model_switch");
    assert_eq!(encoded["detail"]["from"], "anthropic/opus");
    assert_eq!(encoded["detail"]["to"], "openai/gpt");
    assert_eq!(encoded["detail"]["reason"], "fallback");
    let review =
        UiEvent::new(EventKind::Coach, "Advisor warning: x").with_detail(EventDetail::Review {
            severity: "warning".into(),
            note: "x".into(),
        });
    let encoded = serde_json::to_value(review).unwrap();
    assert_eq!(encoded["detail"]["type"], "review");
    assert_eq!(encoded["detail"]["severity"], "warning");
}
