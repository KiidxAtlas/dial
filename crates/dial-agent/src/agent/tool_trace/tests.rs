use super::{CallObservation, MAX_CALL_RECORDS, ToolTraceCounts, UNOFFERED_TOOL};
use crate::agent::*;
use serde_json::json;

fn schema(name: &str) -> serde_json::Value {
    json!({"type": "function", "function": {"name": name, "parameters": {}}})
}

#[test]
fn offered_names_count_once_per_request_and_accumulate_across_requests() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[schema("read"), schema("read"), schema("find")]);
    trace.record_offered(&[schema("read")]);
    assert_eq!(trace.successful_requests, 2);
    assert_eq!(trace.offered.get("read"), Some(&2));
    assert_eq!(trace.offered.get("find"), Some(&1));
}

#[test]
fn malformed_schema_entries_are_ignored_without_failing_the_request() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[
        json!({"function": {"name": 7}}),
        json!("bare"),
        schema("edit"),
    ]);
    assert_eq!(trace.successful_requests, 1);
    assert_eq!(
        trace.offered.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["edit"]
    );
}

#[test]
fn invoked_counts_are_separate_from_offers_and_per_tool() {
    let mut trace = ToolTraceCounts::default();
    trace.record_offered(&[schema("bash")]);
    trace.record_invoked("bash");
    trace.record_invoked("bash");
    assert_eq!(trace.invoked.get("bash"), Some(&2));
    assert_eq!(trace.offered.get("bash"), Some(&1));
    assert!(!trace.invoked.contains_key("find"));
}

fn sse_reply(text: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn sse_tool_call(id: &str, name: &str, arguments: &str) -> crate::llm::fake::Script {
    let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":"tool_calls"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// Runs the given user turns in one hermetic agent and returns its cumulative tool trace plus the
/// number of provider requests the scripted server received.
fn session_trace(
    prompts: &[&str],
    scripts: Vec<crate::llm::fake::Script>,
) -> (ToolTraceCounts, usize) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\n").unwrap();
    session_in(&workspace, prompts, scripts)
}

/// Runs the turns inside a workspace the caller owns, so it can inspect files afterwards.
fn session_in(
    workspace: &tempfile::TempDir,
    prompts: &[&str],
    scripts: Vec<crate::llm::fake::Script>,
) -> (ToolTraceCounts, usize) {
    let config = Config::default();
    let server = crate::llm::fake::serve(scripts);
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        Agent::hermetic(&config, backend, &version, workspace.path(), None, None).unwrap();
    for prompt in prompts {
        agent.turn(prompt, &mut NullUi).unwrap();
    }
    let trace = agent.metrics().tool_trace.clone();
    drop(agent);
    (trace, server.requests().len())
}

#[test]
fn admitted_execution_is_counted_and_every_successful_reply_offers_the_schemas() {
    let (trace, requests) = session_trace(
        &["hi"],
        vec![
            sse_tool_call("call_read", "read", r#"{"path":"notes.txt"}"#),
            sse_reply("done"),
        ],
    );
    assert_eq!(trace.invoked.get("read"), Some(&1));
    assert_eq!(trace.successful_requests, requests as u64);
    let offered = trace.offered.get("read").copied().unwrap_or(0);
    assert!(
        (1..=requests as u64).contains(&offered),
        "offered on {offered} of {requests} replies"
    );
}

#[test]
fn rejected_calls_are_offered_context_but_never_counted_as_invoked() {
    let (trace, requests) = session_trace(
        &["hi"],
        vec![
            sse_tool_call("call_missing", "tool_that_does_not_exist", "{}"),
            sse_reply("done"),
        ],
    );
    assert!(
        !trace.invoked.contains_key("tool_that_does_not_exist"),
        "a rejected form must not count as an execution"
    );
    assert!(trace.invoked.is_empty());
    assert_eq!(trace.successful_requests, requests as u64);
}

#[test]
fn multi_turn_trace_is_cumulative_across_turns_in_one_runtime() {
    let (trace, requests) = session_trace(
        &["first", "second"],
        vec![
            sse_tool_call("call_read", "read", r#"{"path":"notes.txt"}"#),
            sse_reply("first done"),
            sse_reply("second done"),
        ],
    );
    assert_eq!(requests, 3);
    assert_eq!(trace.successful_requests, 3);
    assert_eq!(trace.invoked.get("read"), Some(&1));
    let offered = trace.offered.get("read").copied().unwrap_or(0);
    assert!(
        (1..=3).contains(&offered),
        "offered on {offered} of 3 replies"
    );
}

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn observation<'a>(
    tool: &'a str,
    arguments: &'a serde_json::Value,
    known_keys: &'a [String],
    is_error: bool,
    error_text: &'a str,
) -> CallObservation<'a> {
    CallObservation {
        known_keys,
        tool,
        offered: true,
        arguments,
        seconds: 0.25,
        is_error,
        error_text,
        executed: true,
        reused: false,
    }
}

#[test]
fn call_records_keep_names_and_digests_but_never_raw_values_or_error_text() {
    let mut trace = ToolTraceCounts::default();
    let arguments = json!({"path": "creds-dir/token.txt", "content": "TOKEN-SECRET-123"});
    let known = keys(&["content", "path"]);
    trace.record_call(observation(
        "write",
        &arguments,
        &known,
        true,
        "denied writing TOKEN-SECRET-123",
    ));
    let record = &trace.calls[0];
    assert_eq!(record.ordinal, 1);
    assert_eq!(record.tool, "write");
    assert_eq!(record.input_keys, vec!["content", "path"]);
    assert_eq!(record.input_keys_omitted, 0);
    assert_eq!(record.duration_ms, 250);
    assert_eq!((record.outcome, record.cache), ("error", "none"));
    assert!(record.error_sha256.is_some());
    let serialized = serde_json::to_string(&trace).unwrap();
    for secret in ["TOKEN-SECRET-123", "creds-dir/token.txt", "denied writing"] {
        assert!(
            !serialized.contains(secret),
            "trace contains a value that should have been redacted"
        );
    }
}

/// Argument names the offered schema does not declare can carry secrets, so only their count is
/// kept. Declared names survive; an undeclared (or unoffered) tool keeps none.
#[test]
fn argument_names_outside_the_offered_schema_are_counted_not_recorded() {
    const SECRET_NAME: &str = "SECRET-KEY-NAME-789";
    let table: [(&[&str], serde_json::Value, &[&str], u64); 3] = [
        (&["path"], json!({"path": "a.txt"}), &["path"], 0),
        (
            &["path"],
            json!({"path": "a.txt", "SECRET-KEY-NAME-789": "v"}),
            &["path"],
            1,
        ),
        (
            &[],
            json!({"path": "a.txt", "SECRET-KEY-NAME-789": "v"}),
            &[],
            2,
        ),
    ];
    for (declared, arguments, expected_keys, expected_omitted) in table {
        let known = keys(declared);
        let mut trace = ToolTraceCounts::default();
        trace.record_call(observation("read", &arguments, &known, false, ""));
        let record = &trace.calls[0];
        assert_eq!(record.input_keys, keys(expected_keys), "{arguments}");
        assert_eq!(record.input_keys_omitted, expected_omitted, "{arguments}");
        assert!(!serde_json::to_string(&trace).unwrap().contains(SECRET_NAME));
    }
}

#[test]
fn identical_inputs_share_a_digest_and_distinct_inputs_do_not() {
    let mut trace = ToolTraceCounts::default();
    let first = json!({"path": "a.txt"});
    let second = json!({"path": "b.txt"});
    let known = keys(&["path"]);
    trace.record_call(observation("read", &first, &known, false, ""));
    trace.record_call(observation("read", &first, &known, false, ""));
    trace.record_call(observation("read", &second, &known, false, ""));
    assert_eq!(trace.calls[0].input_sha256, trace.calls[1].input_sha256);
    assert_ne!(trace.calls[0].input_sha256, trace.calls[2].input_sha256);
    assert_eq!(trace.calls[0].error_sha256, None);
}

#[test]
fn unoffered_names_are_generic_and_the_record_list_is_bounded() {
    let mut trace = ToolTraceCounts::default();
    let arguments = json!({});
    let known = keys(&[]);
    let mut unoffered = observation("attacker-chosen-name", &arguments, &known, true, "x");
    unoffered.offered = false;
    trace.record_call(unoffered);
    assert_eq!(trace.calls[0].tool, UNOFFERED_TOOL);
    assert!(
        !serde_json::to_string(&trace)
            .unwrap()
            .contains("attacker-chosen-name")
    );

    for _ in 0..MAX_CALL_RECORDS + 3 {
        trace.record_call(observation("read", &arguments, &known, false, ""));
    }
    assert_eq!(trace.calls.len(), MAX_CALL_RECORDS);
    assert_eq!(trace.calls_omitted, 4);
    assert_eq!(trace.calls.last().unwrap().ordinal, MAX_CALL_RECORDS as u64);
}

/// One core tool exercised by a single admitted call through the real dispatcher.
struct CoreSmoke {
    name: &'static str,
    arguments: &'static str,
    outcome: &'static str,
    /// Declared argument names the call must keep in its record, sorted.
    keys: &'static [&'static str],
    /// Workspace file expected after the call, as (path, exact content).
    effect: Option<(&'static str, &'static str)>,
}

/// Local, service-free core families. `edit` uses the Replace form (`old_string`/`new_string`),
/// which needs no hashline anchors and so can run under a fixed scripted reply.
const CORE_SMOKE: &[CoreSmoke] = &[
    CoreSmoke {
        name: "read",
        arguments: r#"{"path":"notes.txt"}"#,
        outcome: "ok",
        keys: &["path"],
        effect: None,
    },
    CoreSmoke {
        name: "read",
        arguments: r#"{"path":"missing.txt"}"#,
        outcome: "error",
        keys: &["path"],
        effect: None,
    },
    CoreSmoke {
        name: "grep",
        arguments: r#"{"pattern":"alpha"}"#,
        outcome: "ok",
        keys: &["pattern"],
        effect: None,
    },
    CoreSmoke {
        name: "glob",
        arguments: r#"{"pattern":"*.txt"}"#,
        outcome: "ok",
        keys: &["pattern"],
        effect: None,
    },
    CoreSmoke {
        name: "write",
        arguments: r#"{"path":"made.txt","content":"WRITE-SECRET-456\n"}"#,
        outcome: "ok",
        keys: &["content", "path"],
        effect: Some(("made.txt", "WRITE-SECRET-456\n")),
    },
    CoreSmoke {
        name: "edit",
        arguments: r#"{"path":"notes.txt","old_string":"alpha","new_string":"beta"}"#,
        outcome: "ok",
        keys: &["new_string", "old_string", "path"],
        effect: Some(("notes.txt", "beta\n")),
    },
    CoreSmoke {
        name: "bash",
        arguments: r#"{"command":"echo smoke-output"}"#,
        outcome: "ok",
        keys: &["command"],
        effect: None,
    },
];

#[test]
fn core_tools_execute_through_dispatch_and_each_call_leaves_one_redacted_record() {
    for case in CORE_SMOKE {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("notes.txt"), "alpha\n").unwrap();
        // Edits must read the touched file first; that read is a separate, traced call.
        let mut scripts = Vec::new();
        if case.name == "edit" {
            scripts.push(sse_tool_call(
                "call_prior_read",
                "read",
                r#"{"path":"notes.txt"}"#,
            ));
        }
        let prior_reads = scripts.len();
        scripts.push(sse_tool_call("call_core", case.name, case.arguments));
        scripts.push(sse_reply("done"));
        let (trace, _) = session_in(&workspace, &["run the tool"], scripts);
        let label = format!("{} {}", case.name, case.arguments);
        assert_eq!(trace.calls.len(), prior_reads + 1, "{label}");
        if prior_reads == 1 {
            assert_eq!(
                (trace.calls[0].tool.as_str(), trace.calls[0].outcome),
                ("read", "ok")
            );
        }
        let record = trace.calls.last().unwrap();
        assert_eq!(record.tool, case.name, "{label}");
        assert_eq!(record.outcome, case.outcome, "{label}");
        assert_eq!(
            record.error_sha256.is_some(),
            case.outcome == "error",
            "{label}"
        );
        assert_eq!(record.input_keys, keys(case.keys), "{label}");
        assert_eq!(record.input_keys_omitted, 0, "{label}");
        assert!(record.executed, "{label}");
        assert_eq!(trace.invoked.get(case.name), Some(&1), "{label}");
        if let Some((path, content)) = case.effect {
            assert_eq!(
                std::fs::read_to_string(workspace.path().join(path)).unwrap(),
                content,
                "{label}"
            );
        }
        let serialized = serde_json::to_string(&trace).unwrap();
        let arguments: serde_json::Value = serde_json::from_str(case.arguments).unwrap();
        for value in arguments
            .as_object()
            .unwrap()
            .values()
            .filter_map(|v| v.as_str())
        {
            assert!(
                !serialized.contains(value),
                "{label}: argument value leaked"
            );
        }
    }
}

/// An invented argument name is dispatched as the model sent it, but its name stays out of the trace.
#[test]
fn invented_argument_names_reach_dispatch_without_entering_the_trace() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "alpha\n").unwrap();
    let (trace, _) = session_in(
        &workspace,
        &["hi"],
        vec![
            sse_tool_call(
                "call_extra",
                "read",
                r#"{"path":"notes.txt","SECRET-KEY-NAME-789":"v"}"#,
            ),
            sse_reply("done"),
        ],
    );
    assert_eq!(trace.calls.len(), 1);
    assert_eq!(trace.calls[0].tool, "read");
    assert_eq!(trace.calls[0].input_keys, vec!["path"]);
    assert_eq!(trace.calls[0].input_keys_omitted, 1);
    assert!(
        !serde_json::to_string(&trace)
            .unwrap()
            .contains("SECRET-KEY-NAME-789")
    );
}

#[test]
fn a_name_that_was_not_offered_is_refused_and_recorded_generically() {
    let workspace = tempfile::tempdir().unwrap();
    let (trace, _) = session_in(
        &workspace,
        &["hi"],
        vec![
            sse_tool_call("call_unknown", "nonexistent_tool_xyz", "{}"),
            sse_reply("done"),
        ],
    );
    assert_eq!(trace.calls.len(), 1);
    assert_eq!(trace.calls[0].tool, UNOFFERED_TOOL);
    assert!(!trace.calls[0].executed);
    assert_eq!(trace.calls[0].outcome, "error");
    assert!(trace.invoked.is_empty());
    assert!(
        !serde_json::to_string(&trace)
            .unwrap()
            .contains("nonexistent_tool_xyz")
    );
}

#[test]
fn a_repeated_read_of_an_unchanged_file_is_traced_as_cache_reuse() {
    let workspace = tempfile::tempdir().unwrap();
    // Reuse only replaces a read whose rows cost more than the pointer (investigation.rs), so the
    // fixture must be larger than the pointer text.
    let source: String = (1..=60)
        .map(|line| format!("row {line:03} of the unchanged source\n"))
        .collect();
    std::fs::write(workspace.path().join("notes.txt"), source).unwrap();
    // A line-selected first read registers its rows; a narrower later read is answered from them.
    let first_args = r#"{"path":"notes.txt:1-60"}"#;
    let second_args = r#"{"path":"notes.txt:10-20"}"#;
    let (trace, _) = session_in(
        &workspace,
        &["read twice"],
        vec![
            sse_tool_call("call_first", "read", first_args),
            sse_tool_call("call_second", "read", second_args),
            sse_reply("done"),
        ],
    );
    let [first_call, second_call] = trace.calls.as_slice() else {
        panic!("expected two traced reads, got {:?}", trace.calls);
    };
    assert_eq!((first_call.cache, first_call.executed), ("none", true));
    assert_ne!(first_call.input_sha256, second_call.input_sha256);
    assert_eq!((second_call.cache, second_call.executed), ("reused", false));
    assert_eq!(
        trace.invoked.get("read"),
        Some(&1),
        "a reuse is not an execution"
    );
}
