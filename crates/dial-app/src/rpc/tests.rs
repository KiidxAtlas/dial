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
    assert!(alert.get("detail").is_none());
    let switch = typed_notice(&UiEvent::new(EventKind::Model, "fallback").with_detail(
        crate::agent::ui::ui_event::EventDetail::ModelSwitch {
            from: None,
            to: "openai/gpt".into(),
            reason: crate::agent::ui::ui_event::SwitchReason::Fallback,
        },
    ));
    assert_eq!(switch["detail"]["type"], "model_switch");
    assert_eq!(switch["detail"]["to"], "openai/gpt");
    assert!(switch["detail"]["from"].is_null());
}

#[test]
fn retry_boundary_and_call_correlated_tool_events_have_wire_shape() {
    use crate::agent::ui::ui_event::{EventKind, UiEvent};
    let retry = typed_notice(&UiEvent::new(EventKind::Retry, "partial discarded"));
    assert_eq!(retry["event"], "notice");
    assert_eq!(retry["kind"], "retry");
    assert_eq!(retry["lane"], "context");
    assert_eq!(retry["message"], "partial discarded");
    let output = super::tool_output_event("call_2", "bash", "out-2\n");
    assert_eq!(output["event"], "tool_output");
    assert_eq!(output["call_id"], "call_2");
    assert_eq!(output["name"], "bash");
    assert_eq!(output["delta"], "out-2\n");
    let progress = super::tool_progress_event("call_1", "bash", "step 2");
    assert_eq!(progress["event"], "tool_progress");
    assert_eq!(progress["call_id"], "call_1");
    assert_eq!(progress["text"], "step 2");
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
    for source in [
        include_str!("../rpc.rs"),
        include_str!("actor.rs"),
        include_str!("jobs.rs"),
    ] {
        for line in source.lines() {
            let Some((pattern, _)) = line.trim().split_once(" => ") else {
                continue;
            };
            if pattern.ends_with(" if acp") {
                continue;
            }
            // Native-only arms are still dispatched methods.
            let pattern = pattern.strip_suffix(" if !acp").unwrap_or(pattern);
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
    for source in [
        include_str!("../rpc.rs"),
        include_str!("actor.rs"),
        include_str!("host_tools.rs"),
        include_str!("host_uris.rs"),
        include_str!("auth.rs"),
        include_str!("extensions.rs"),
        include_str!("side_control.rs"),
    ] {
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
    let table = names(METHODS.iter().map(|method| method.name.to_owned()));
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
    // Connections start on protocol 1 until the client sends `hello`.
    assert_eq!(rpc["protocol_version"], 1);
    assert_eq!(
        rpc["supported_protocol_versions"],
        json!([1, PROTOCOL_VERSION])
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
    // A client negotiates the structured Type context on this flag, not on the engine version.
    assert_eq!(rpc["features"]["type_context"], json!(true));

    let acp = ready_frame("s1", "m", json!([]), false, true);
    assert_eq!(acp["limits"]["max_request_bytes"], MAX_ACP_REQUEST);
    assert!(!capability(&acp, "questions") && !capability(&acp, "live_voice"));
    assert!(capability(&acp, "prompt"));
    // The new workflow capabilities are native-only: ACP never announces or serves them.
    for capability_name in [
        "checks",
        "evidence",
        "diagnostics",
        "skills",
        "peer_coordination",
    ] {
        assert!(!capability(&acp, capability_name), "{capability_name}");
        assert!(capability(&rpc, capability_name), "{capability_name}");
    }
    let acp_methods = acp["methods"].as_array().unwrap();
    for method in [
        "list_checks",
        "add_check",
        "remove_check",
        "evidence",
        "evidence_correct",
        "evidence_compare",
        "policy",
        "reliability",
        "list_skills",
        "skill_action",
        "live_speak",
        "list_peers",
        "send_peer_message",
    ] {
        assert!(!acp_methods.contains(&json!(method)), "{method}");
        assert!(
            rpc["methods"].as_array().unwrap().contains(&json!(method)),
            "{method}"
        );
    }
    let methods = acp["methods"].as_array().unwrap();
    assert!(!methods.contains(&json!("live_start")) && methods.contains(&json!("send")));
    assert_eq!(acp["question_input"]["supported"], false);
    assert!(capability(&rpc, "host_uris") && capability(&rpc, "auth"));
    assert!(!capability(&acp, "host_tools") && !capability(&acp, "host_uris"));
    assert!(!capability(&acp, "auth"));
}

#[test]
fn hello_picks_the_highest_version_both_sides_speak() {
    assert_eq!(negotiated_version(1).unwrap(), 1);
    assert_eq!(negotiated_version(2).unwrap(), 2);
    assert_eq!(negotiated_version(9).unwrap(), PROTOCOL_VERSION);
}

#[test]
fn hello_below_every_served_version_is_refused_with_a_code() {
    let err = negotiated_version(0).unwrap_err();
    let error = err.downcast_ref::<RequestError>().unwrap();
    assert_eq!(error.code, ErrorCode::UnsupportedProtocolVersion);
}

#[test]
fn hello_flags_default_when_absent_and_refuse_non_booleans() {
    let params = json!({"protocol_version": 2, "tree_events": false, "interactive": null});
    assert!(!hello_flag(&params, "tree_events", true).unwrap());
    assert!(
        !hello_flag(&params, "interactive", false).unwrap(),
        "null is the default"
    );
    assert!(
        hello_flag(&json!({}), "tree_events", true).unwrap(),
        "tree events stay on by default"
    );
    let err = hello_flag(&json!({"tree_events": "no"}), "tree_events", true).unwrap_err();
    let error = err.downcast_ref::<RequestError>().unwrap();
    assert_eq!(error.code, ErrorCode::InvalidParams);
    assert_eq!(error.message, "tree_events must be a boolean");
}

#[test]
fn only_a_completed_top_level_inference_ends_a_model_request() {
    let node = |kind: &str, parent: Option<&str>, phase: &str| -> crate::runtime::Node {
        serde_json::from_value(json!({
            "id": "n", "parent": parent, "kind": kind, "label": "",
            "result": {"text": "", "phase": phase, "revision": 1}
        }))
        .unwrap()
    };
    assert!(ends_model_request(&node("inference", None, "completed")));
    assert!(!ends_model_request(&node("inference", None, "running")));
    assert!(!ends_model_request(&node("inference", None, "failed")));
    assert!(
        !ends_model_request(&node("inference", Some("worker-1"), "completed")),
        "a child's request"
    );
    assert!(!ends_model_request(&node("tool", None, "completed")));
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
        assert!(METHODS.iter().any(|method| frame["method"] == method.name));
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
fn collaboration_turn_schema_requires_one_complete_outcome() {
    let schema = schema();
    for (payload, valid) in [
        (
            json!({"outcome":"completed","result":{"local_turn":true}}),
            true,
        ),
        (
            json!({"outcome":"failed","error":{"code":"refused","message":"not admitted"}}),
            true,
        ),
        (json!({"outcome":"completed"}), false),
        (json!({"outcome":"failed"}), false),
        (
            json!({"outcome":"completed","result":[],"error":null}),
            false,
        ),
        (
            json!({"outcome":"completed","result":{},"error":{"code":"failed","message":"mixed"}}),
            false,
        ),
        (
            json!({"outcome":"failed","result":{},"error":{"code":"failed","message":"mixed"}}),
            false,
        ),
        (
            json!({"outcome":"failed","error":{"message":"missing code"}}),
            false,
        ),
        (json!({"outcome":"failed","error":{"code":"failed"}}), false),
        (
            json!({"outcome":"failed","error":{"code":"failed","message":"bad","secret":"unexpected"}}),
            false,
        ),
    ] {
        let mut event =
            json!({"event":"collaboration_turn","seq":1,"session_id":"s","request_id":null});
        event
            .as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
        let result = crate::runtime::schema::validate(&schema, &event);
        assert_eq!(result.is_ok(), valid, "event={event}: {result:?}");
    }
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

#[test]
fn only_listed_methods_are_known_before_dispatch() {
    assert!(known_method("state"));
    assert!(known_method("send"));
    assert!(!known_method("nope"));
    assert!(!known_method(""));
    // ACP-internal actor methods are not native RPC methods.
    assert!(!known_method("close_session"));
    assert!(known_method("fork_session") && known_method("list_sessions"));
    assert!(known_method("search_sessions"));
    assert!(Admin::reads("search_sessions"));
    assert!(dial_protocol::is_read_only("search_sessions"));
    let native = ready_frame("s", "m", json!([]), false, false);
    assert!(
        native["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("search_sessions"))
    );
    let acp = ready_frame("s", "m", json!([]), false, true);
    assert!(
        !acp["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("search_sessions"))
    );
}

#[test]
fn envelope_numbers_events_and_attributes_them_without_touching_replies() {
    let mut envelope = Envelope::default();
    let mut ready = ready_frame("s1", "m", json!([]), false, false);
    assert!(
        ready["engine_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
    );
    envelope.stamp(&mut ready);
    assert_eq!(ready["seq"], 1);
    assert_eq!(ready["session_id"], "s1");
    assert_eq!(ready["request_id"], Value::Null);

    envelope.session_id = Some("s2".into());
    envelope.request_id = json!("turn-7");
    let mut text = json!({"event":"text","delta":"hi"});
    envelope.stamp(&mut text);
    assert_eq!(
        text,
        json!({"event":"text","delta":"hi","seq":2,"session_id":"s2","request_id":"turn-7"})
    );
    // An event that names its own session keeps it.
    let mut closed = json!({"event":"question_closed","session_id":"other"});
    envelope.stamp(&mut closed);
    assert_eq!(
        (closed["seq"].clone(), closed["session_id"].clone()),
        (json!(3), json!("other"))
    );
    // Replies are correlated by id and are never numbered.
    let mut reply = json!({"id":1,"result":{}});
    envelope.stamp(&mut reply);
    assert_eq!(reply, json!({"id":1,"result":{}}));
    assert_eq!(envelope.seq, 3);
}

#[test]
fn ready_and_side_events_cannot_inherit_stale_session_or_turn_attribution() {
    let mut envelope = Envelope {
        session_id: Some("old".into()),
        request_id: json!("old-turn"),
        ..Default::default()
    };
    let mut ready = ready_frame("new", "m", json!([]), false, false);
    envelope.stamp(&mut ready);
    assert!(ready["request_id"].is_null());
    let mut text = json!({"event":"text","delta":"new work"});
    envelope.stamp(&mut text);
    assert_eq!(text["session_id"], "new");
    envelope.request_id = json!("new-turn");
    let mut side = json!({"event":"side_state","session_id":"old","request_id":null,"state":{"threads":[]},"notices":[]});
    envelope.stamp(&mut side);
    assert_eq!(side["session_id"], "old");
    assert!(side["request_id"].is_null());
    let mut next = json!({"event":"text","delta":"still new"});
    envelope.stamp(&mut next);
    assert_eq!(next["session_id"], "new");
    assert_eq!(next["request_id"], "new-turn");
    let mut active = json!({"event":"active_session","session_id":"third"});
    envelope.stamp(&mut active);
    envelope.stamp(&mut next);
    // Explicitly attributed events keep their original owner, even after a switch.
    assert_eq!(next["session_id"], "new");
    let mut third = json!({"event":"notice","message":"current"});
    envelope.stamp(&mut third);
    assert_eq!(third["session_id"], "third");
}

#[test]
fn list_extensions_reports_configured_and_curated_rows() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let mut config = Config {
        user_dir: layout.user_dir.clone(),
        project_root: layout.project_root.clone(),
        ..Config::default()
    };
    config.mcp.insert(
        "mine".into(),
        crate::config::types::McpServerConfig {
            enabled: false,
            ..Default::default()
        },
    );
    let listed = extensions::list(&layout, &config);
    let mcp = listed["mcp"].as_array().unwrap();
    let mine = mcp.iter().find(|row| row["id"] == "mine").unwrap();
    assert_eq!(
        (mine["origin"].as_str(), mine["state"].as_str()),
        (Some("installed"), Some("disabled"))
    );
    assert!(mcp.iter().any(|row| row["origin"] == "curated"));
    assert!(listed["skills"].is_array());
}
#[test]
fn a_spoken_reply_is_bracketed_by_live_speech_events_even_when_it_fails() {
    let env = crate::voice::VoiceEnv {
        voice: Default::default(),
        user_dir: std::env::temp_dir(),
    };
    let session = crate::voice::live::start(&env, None, None).unwrap();
    let events = Mutex::new(Vec::new());
    // Blank text fails before any speech program is chosen, so no audio is produced.
    read_aloud(&session, "  ", &Arc::new(AtomicBool::new(false)), |event| {
        events.lock().unwrap().push(event);
    });
    let events = events.into_inner().unwrap();
    let names: Vec<_> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["live_speech", "live_speech", "live_error"]);
    assert_eq!(events[0]["speaking"], true);
    assert_eq!(events[1]["speaking"], false);
    assert_eq!(events[1]["cancelled"], false);
}

#[test]
fn a_failed_turn_replies_with_its_diagnostic_and_not_the_guidance_written_for_the_model() {
    let failure = || {
        crate::agent::test_provider_failure(
            anyhow::anyhow!("Responses error (code: model_not_found)")
                .context("reading chat stream"),
        )
    };
    assert!(format!("{:#}", failure()).starts_with("Recovery state:"));
    let diagnostic = "reading chat stream: Responses error (code: model_not_found)";
    assert_eq!(
        Sender::result_frame(&json!(7), Err(failure()), 2),
        json!({"id":7,"error":{"code":"failed","message":diagnostic}})
    );
    assert_eq!(
        Sender::result_frame(&json!(7), Err(failure()), 1),
        json!({"id":7,"error":{"message":diagnostic}})
    );
    // A coded request failure keeps its code and its message.
    assert_eq!(
        Sender::result_frame(
            &json!(8),
            Err(coded(ErrorCode::Busy, "workers are running")),
            2
        ),
        json!({"id":8,"error":{"code":ErrorCode::Busy.as_str(),"message":"workers are running"}})
    );
}
