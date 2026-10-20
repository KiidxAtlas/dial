use super::*;
use serde_json::json;

#[test]
fn request_settings_changes_update_status_without_result_notice_spam() {
    let mut status = crate::ui::status::StatusData::default();
    let first = json!({"adaptive_policy":{"model":"model-a","selection":{"effective":{
        "temperature":0.6,"thinking":true,"max_tokens":4096,"sampling_source":"model metadata"
    }},"result":{"state":"awaiting response"}}});
    assert!(request_settings_change(&mut status, &Value::Null, &first).is_some());
    let before = status.inference.clone();
    let mut received = first.clone();
    received["adaptive_policy"]["result"] =
        json!({"state":"response received","prompt_tokens":200});
    assert!(request_settings_change(&mut status, &first, &received).is_none());
    assert_eq!(status.inference, before);
    let mut next = received.clone();
    next["adaptive_policy"]["selection"]["effective"]["thinking"] = json!(false);
    assert!(request_settings_change(&mut status, &received, &next).is_some());
    assert_ne!(status.inference, before);
    let mut switched = next.clone();
    switched["adaptive_policy"]["model"] = json!("model-b");
    assert!(request_settings_change(&mut status, &next, &switched).is_some());
    assert!(request_settings_change(&mut status, &switched, &Value::Null).is_none());
    assert!(status.inference.is_empty());
}
