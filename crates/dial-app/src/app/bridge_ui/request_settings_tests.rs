use super::*;
use serde_json::json;

#[test]
fn request_settings_changes_update_status_without_result_notice_spam() {
    let mut status = crate::ui::status::StatusData::default();
    let first = json!({"adaptive_policy":{"model":"model-a","selection":{"effective":{
        "temperature":0.6,"thinking":true,"max_tokens":4096,"sampling_source":"model metadata"
    }},"result":{"state":"awaiting response"}}});
    // The first request only fills the status line; the transcript stays quiet.
    assert!(request_settings_change(&mut status, &Value::Null, &first).is_none());
    assert!(!status.inference.is_empty());
    let before = status.inference.clone();
    let mut received = first.clone();
    received["adaptive_policy"]["result"] =
        json!({"state":"response received","prompt_tokens":200});
    assert!(request_settings_change(&mut status, &first, &received).is_none());
    assert_eq!(status.inference, before);
    let mut next = received.clone();
    next["adaptive_policy"]["selection"]["effective"]["thinking"] = json!(false);
    assert_eq!(
        request_settings_change(&mut status, &received, &next).as_deref(),
        Some("thinking on → off · /policy for details")
    );
    assert_ne!(status.inference, before);
    let mut switched = next.clone();
    switched["adaptive_policy"]["model"] = json!("model-b");
    // A model switch has its own notice; settings for the new model are not a "change".
    assert!(request_settings_change(&mut status, &next, &switched).is_none());
    assert!(request_settings_change(&mut status, &switched, &Value::Null).is_none());
    assert!(status.inference.is_empty());
}
