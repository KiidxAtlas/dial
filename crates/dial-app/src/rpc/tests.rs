use super::*;
#[test]
fn oversized_request_is_drained_without_consuming_the_next_request() {
    let mut bytes = vec![b'x'; MAX_REQUEST + 100];
    bytes.extend_from_slice(b"\n{\"id\":1,\"method\":\"state\"}\n");
    let mut input = std::io::BufReader::with_capacity(32, std::io::Cursor::new(bytes));
    assert!(read_request(&mut input).unwrap().unwrap().is_err());
    let next = read_request(&mut input).unwrap().unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&next).unwrap()["method"],
        "state"
    );
    assert!(read_request(&mut input).unwrap().is_none());
}
#[test]
fn complete_request_without_final_newline_is_accepted() {
    let mut input = std::io::Cursor::new(b"{\"id\":2,\"method\":\"state\"}");
    let next = read_request(&mut input).unwrap().unwrap().unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&next).unwrap()["id"], 2);
}
#[test]
fn state_reply_reads_only_the_state_projection() {
    let projection = QueryProjection {
        state: json!({"session_id":"s1","tree":[]}),
        messages: json!([{"role":"user","content":"large history"}]),
    };
    let reply = state_reply(&projection, true).unwrap();
    assert_eq!(reply, json!({"session_id":"s1","tree":[],"busy":true}));
    assert!(reply.get("messages").is_none());
    assert!(state_reply(&QueryProjection::default(), false).is_err());
    assert_eq!(QueryProjection::default().messages, json!([]));
}
#[test]
fn history_pages_preserve_boundaries_and_reject_invalid_cursors() {
    let messages: Vec<_> = (0..600).map(|n| json!({"content":n})).collect();
    assert_eq!(
        messages_page(&messages, &Value::Null).unwrap()["next_offset"],
        50
    );
    let page = messages_page(&messages, &json!({"cursor":"599","limit":u64::MAX})).unwrap();
    assert_eq!(
        page,
        json!({"messages":[{"content":599}],"offset":599,"total":600,"next_offset":null})
    );
    assert_eq!(
        messages_page(&messages, &json!({"offset":600})).unwrap()["messages"],
        json!([])
    );
    assert_eq!(
        messages_page(&messages, &json!({"limit":0})).unwrap()["next_offset"],
        1
    );
    assert_eq!(
        messages_page(&messages, &json!({"limit":u64::MAX})).unwrap()["next_offset"],
        500
    );
    assert_eq!(
        messages_page(&messages, &json!({"offset":2,"cursor":"4"})).unwrap()["offset"],
        2
    );
    for params in [
        json!({"cursor":"bad"}),
        json!({"cursor":-1}),
        json!({"offset":-1}),
        json!({"offset":"2"}),
        json!({"offset":1.5}),
        json!({"offset":u64::MAX}),
        json!({"cursor":"18446744073709551616"}),
        json!({"limit":-1}),
        json!({"limit":"2"}),
        json!([]),
    ] {
        assert!(messages_page(&messages, &params).is_err(), "{params}");
    }
    assert_eq!(
        messages_page(&[], &json!({})).unwrap(),
        json!({"messages":[],"offset":0,"total":0,"next_offset":null})
    );
}

#[test]
fn typed_notice_adds_lane_and_keeps_plain_text_and_tokens() {
    use crate::agent::ui_event::{EventKind, UiEvent};
    let value = typed_notice(
        &UiEvent::new(EventKind::Compaction, "History compacted").with_tokens(900, 120),
    );
    assert_eq!(value["event"], "notice");
    assert_eq!(value["message"], "History compacted");
    assert_eq!(value["kind"], "compaction");
    assert_eq!(value["lane"], "divider");
    assert_eq!(value["tokens"], json!([900, 120]));
    let alert = typed_notice(&UiEvent::new(EventKind::Recovery, "retrying"));
    assert_eq!(alert["lane"], "alert");
    assert!(alert["tokens"].is_null());
}

#[test]
fn node_metadata_ignores_text_growth_and_tracks_every_other_field() {
    use crate::runtime::{Node, Phase, ResultBody};
    let mut node = Node {
        id: "n1".into(),
        parent: None,
        kind: "tool".into(),
        label: "read".into(),
        call_id: None,
        result: ResultBody {
            execution: None,
            text: "a".into(),
            data: None,
            phase: Phase::Running,
            revision: 1,
        },
        presentation: None,
    };
    let before = node_metadata(&node);
    // Text-only growth keeps the metadata equal, so the observer may send a delta.
    node.result.text.push('b');
    node.result.revision = 2;
    assert_eq!(node_metadata(&node), before);
    // Each metadata field counts as a change that requires a full node.
    node.call_id = Some("c1".into());
    assert_ne!(node_metadata(&node), before);
    node.call_id = None;
    node.result.data = Some(json!({"progress": 1}));
    assert_ne!(node_metadata(&node), before);
    node.result.data = None;
    node.result.phase = Phase::Completed;
    assert_ne!(node_metadata(&node), before);
}
