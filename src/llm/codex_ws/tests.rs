use super::*;

fn user(text: &str) -> Value {
    json!({"role":"user","content":[{"type":"input_text","text":text}]})
}

#[test]
fn delta_requires_strict_prefix_and_equal_options() {
    let previous = json!({"model":"m","input":[user("a")]});
    let output = vec![
        json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[]}),
    ];
    let replayed = json!({"type":"message","role":"assistant","content":[]});
    let current = json!({"model":"m","input":[user("a"), replayed, user("b")]});
    assert_eq!(
        delta_input(&previous, &output, &current),
        Some(vec![user("b")])
    );
    let mutated = json!({"model":"m","input":[user("x"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &mutated), None);
    let other_model = json!({"model":"n","input":[user("a"), json!({"type":"message","role":"assistant","content":[]}), user("b")]});
    assert_eq!(delta_input(&previous, &output, &other_model), None);
}

#[test]
fn steered_plan_attaches_creates_or_discards() {
    let steer = vec![user("stop and do b")];
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b")]), &steer),
        Plan::Attach
    ));
    assert!(
        matches!(plan_steered(Some(&[output.clone(), user("stop and do b")]), &steer), Plan::Create(rest) if rest == vec![output])
    );
    assert!(matches!(
        plan_steered(Some(&[user("stop and do b"), user("unrelated")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(
        plan_steered(Some(&[user("other")]), &steer),
        Plan::Discard
    ));
    assert!(matches!(plan_steered(None, &steer), Plan::Discard));
}

#[test]
fn broken_pipe_write_is_retryable_but_protocol_errors_are_not() {
    let error = write_error(tungstenite::Error::Io(std::io::Error::from(
        ErrorKind::BrokenPipe,
    )));
    assert!(
        error
            .downcast_ref::<super::super::stream::IncompleteStream>()
            .is_some()
    );
    assert!(super::super::errors::classify(&error).is_retryable());
    let error = write_error(tungstenite::Error::Protocol(
        tungstenite::error::ProtocolError::InvalidOpcode(15),
    ));
    assert!(!super::super::errors::classify(&error).is_retryable());
}

#[test]
fn only_plain_user_messages_can_steer() {
    assert!(steer_items(&json!([user("a")])).is_some());
    assert!(
        steer_items(&json!([{"type":"function_call_output","call_id":"c","output":""}])).is_none()
    );
    assert!(steer_items(&json!([])).is_none());
}
