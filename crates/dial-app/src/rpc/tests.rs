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
fn request_limit_is_identical_with_a_newline_or_eof() {
    const LIMIT: usize = 16;
    for capacity in [1, 7, 64] {
        for terminated in [false, true] {
            for length in [LIMIT - 1, LIMIT, LIMIT + 1] {
                let mut bytes = vec![b'x'; length];
                if terminated {
                    bytes.push(b'\n');
                }
                let mut input =
                    std::io::BufReader::with_capacity(capacity, std::io::Cursor::new(bytes));
                let result = read_request_limit(&mut input, LIMIT).unwrap().unwrap();
                assert_eq!(
                    result.is_ok(),
                    length <= LIMIT,
                    "capacity={capacity}, terminated={terminated}, length={length}"
                );
                if let Ok(payload) = result {
                    assert_eq!(payload, vec![b'x'; length]);
                }
                assert!(read_request_limit(&mut input, LIMIT).unwrap().is_none());
            }
        }
    }
}
#[test]
fn state_reply_reads_only_the_state_projection() {
    let projection = QueryProjection {
        state: json!({"session_id":"s1","tree":[]}),
        messages: json!([{"role":"user","content":"large history"}]),
        cursor: Default::default(),
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
    use crate::agent::ui::ui_event::{EventKind, UiEvent};
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

#[test]
fn cached_history_refresh_is_incremental_and_preserves_old_busy_snapshot() {
    let mut session = Session::ephemeral();
    let mut projection = QueryProjection::default();
    session
        .append(json!({"role":"user","content":"first"}))
        .unwrap();
    refresh_messages(&session, &mut projection);
    let old_body = projection.messages[0]["content"].as_str().unwrap().as_ptr();
    session
        .append(json!({"role":"assistant","content":"second"}))
        .unwrap();
    assert_eq!(projection.messages.as_array().unwrap().len(), 1);
    refresh_messages(&session, &mut projection);
    assert_eq!(projection.messages, json!(session.messages()));
    assert_eq!(
        old_body,
        projection.messages[0]["content"].as_str().unwrap().as_ptr()
    );
    session
        .append(json!({"role":"user","content":"abandoned"}))
        .unwrap();
    refresh_messages(&session, &mut projection);
    session.rewind(1).unwrap();
    session
        .append(json!({"role":"user","content":"replacement"}))
        .unwrap();
    refresh_messages(&session, &mut projection);
    assert_eq!(projection.messages, json!(session.messages()));
    session.compact("summary".into(), 1).unwrap();
    refresh_messages(&session, &mut projection);
    assert_eq!(projection.messages, json!(session.messages()));
    refresh_messages(&Session::ephemeral(), &mut projection);
    assert_eq!(projection.messages, json!([]));
}

fn schema() -> Value {
    serde_json::from_str(include_str!("../../../../docs/rpc-schema.json")).unwrap()
}
fn names(values: impl IntoIterator<Item = String>) -> std::collections::BTreeSet<String> {
    values.into_iter().collect()
}
fn schema_consts(branches: &Value, key: &str) -> std::collections::BTreeSet<String> {
    names(branches.as_array().unwrap().iter().map(|branch| {
        branch["properties"][key]["const"]
            .as_str()
            .unwrap()
            .to_owned()
    }))
}
/// Method names matched by the dispatchers: arms made only of quoted identifiers (`"a" | "b" =>`),
/// excluding ACP-only arms.
fn dispatched_methods() -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for source in [include_str!("../rpc.rs"), include_str!("actor.rs")] {
        for line in source.lines() {
            let Some((pattern, _)) = line.trim().split_once(" => ") else {
                continue;
            };
            if pattern.ends_with(" if acp") {
                continue;
            }
            let quoted: Option<Vec<&str>> = pattern
                .split(" | ")
                .map(|part| {
                    let name = part.strip_prefix('"')?.strip_suffix('"')?;
                    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
                        .then_some(name)
                })
                .collect();
            found.extend(quoted.into_iter().flatten().map(str::to_owned));
        }
    }
    found
}
fn emitted_events() -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for source in [include_str!("../rpc.rs"), include_str!("actor.rs")] {
        for piece in source.split("\"event\":\"").skip(1) {
            found.insert(piece.split('"').next().unwrap().to_owned());
        }
    }
    found
}

#[test]
fn schema_requests_match_the_method_table_and_the_dispatchers() {
    let schema = schema();
    let in_schema = schema_consts(&schema["$defs"]["request"]["oneOf"], "method");
    let table = names(METHODS.iter().map(|(method, _)| (*method).to_owned()));
    assert_eq!(in_schema, table, "docs/rpc-schema.json and METHODS differ");
    let dispatched = dispatched_methods()
        .into_iter()
        .filter(|name| !name.starts_with('_'))
        .collect();
    assert_eq!(table, dispatched, "METHODS and the dispatch arms differ");
}

#[test]
fn schema_events_match_the_events_the_server_emits() {
    let schema = schema();
    let in_schema = schema_consts(&schema["$defs"]["event"]["oneOf"], "event");
    assert_eq!(in_schema, emitted_events());
}

#[test]
fn schema_limits_equal_the_enforced_constants() {
    let schema = schema();
    let ready = ready_frame("s", "m", json!([]), false, false);
    let keys = |value: &Value| names(value.as_object().unwrap().keys().cloned());
    assert_eq!(
        keys(&ready["limits"]),
        keys(&schema["$defs"]["limits"]["properties"])
    );
    assert_eq!(
        names(
            schema["$defs"]["limits"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|key| key.as_str().unwrap().to_owned())
        ),
        keys(&ready["limits"])
    );
    let params = |method: &str| -> Value {
        schema["$defs"]["request"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|branch| branch["properties"]["method"]["const"] == method)
            .unwrap()["properties"]["params"]["properties"]
            .clone()
    };
    assert_eq!(
        params("memory")["command"]["maxLength"],
        MAX_MEMORY_COMMAND_BYTES
    );
    assert_eq!(
        params("seed_context")["messages"]["maxItems"],
        MAX_SEED_MESSAGES
    );
    assert_eq!(
        params("messages_page")["limit"]["maximum"],
        MESSAGES_PAGE_MAX
    );
    assert_eq!(
        params("messages_page")["limit"]["default"],
        MESSAGES_PAGE_DEFAULT
    );
}

#[test]
fn ready_frame_announces_versions_limits_and_transport_capabilities() {
    let rpc = ready_frame("s1", "m", json!(["read"]), true, false);
    assert_eq!(rpc["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(
        rpc["supported_protocol_versions"],
        json!([PROTOCOL_VERSION])
    );
    assert_eq!(rpc["limits"]["max_request_bytes"], MAX_REQUEST);
    let capability = |frame: &Value, name: &str| {
        frame["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!(name))
    };
    assert!(capability(&rpc, "questions") && capability(&rpc, "live_voice"));
    assert_eq!(rpc["methods"].as_array().unwrap().len(), METHODS.len());

    let acp = ready_frame("s1", "m", json!([]), false, true);
    assert_eq!(acp["limits"]["max_request_bytes"], MAX_ACP_REQUEST);
    assert!(!capability(&acp, "questions") && !capability(&acp, "live_voice"));
    assert!(capability(&acp, "prompt"));
    let methods = acp["methods"].as_array().unwrap();
    assert!(!methods.contains(&json!("live_start")) && methods.contains(&json!("send")));
    assert_eq!(acp["question_input"]["supported"], false);
}

#[test]
fn announced_request_limit_is_the_one_the_reader_enforces() {
    for acp in [false, true] {
        let limit = request_limit(acp);
        let mut exact = std::io::Cursor::new(vec![b'x'; limit]);
        assert!(
            read_request_limit(&mut exact, limit)
                .unwrap()
                .unwrap()
                .is_ok()
        );
        let mut over = std::io::Cursor::new(vec![b'x'; limit + 1]);
        assert!(
            read_request_limit(&mut over, limit)
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
}

#[test]
fn client_request_examples_validate_against_the_published_schema() {
    let schema = schema();
    crate::runtime::schema::check_schema(&schema).unwrap();
    for line in include_str!("../../../../examples/clients/rpc-v1.jsonl").lines() {
        let frame: Value = serde_json::from_str(line).unwrap();
        crate::runtime::schema::validate(&schema, &frame).unwrap();
        assert!(METHODS.iter().any(|(method, _)| frame["method"] == *method));
        let mut input = std::io::Cursor::new(format!("{line}\n").into_bytes());
        let read = read_request(&mut input).unwrap().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&read).unwrap(), frame);
    }
    // The examples are constrained contracts, not merely parseable JSON.
    assert!(
        crate::runtime::schema::validate(
            &schema,
            &json!({"id":"bad","method":"send","params":{"text":42}})
        )
        .is_err()
    );
}

#[test]
fn revisioned_snapshot_retains_partial_output_and_interrupted_outcome() {
    use crate::runtime::{Phase, Tree};
    let tree = Tree::default();
    let body = tree.start("tool", "read", None);
    body.correlate("call-1");
    let before = body.snapshot();
    body.append("partial λ");
    let partial = body.snapshot();
    assert!(partial.result.revision > before.result.revision);
    assert_eq!(node_metadata(&partial), node_metadata(&before));
    body.settle("partial λ".into(), None, Phase::Interrupted);
    let stopped = body.snapshot();
    assert!(stopped.result.revision > partial.result.revision);
    assert_ne!(node_metadata(&stopped), node_metadata(&partial));
    body.append("late text must not resume a stopped stream");
    assert_eq!(body.snapshot(), stopped);
    let projection = QueryProjection {
        state: json!({"session_id":"s1","tree":[stopped]}),
        ..Default::default()
    };
    let restored = state_reply(&projection, false).unwrap();
    assert_eq!(restored["tree"][0]["result"]["text"], "partial λ");
    assert_eq!(restored["tree"][0]["result"]["phase"], "interrupted");
    assert_eq!(restored["tree"][0]["call_id"], "call-1");
}
