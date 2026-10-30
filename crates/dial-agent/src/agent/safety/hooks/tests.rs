use crate::config::hooks::validate;

use super::*;
#[cfg(unix)]
#[test]
fn presentation_hooks_cannot_clear_execution_uncertainty_or_committed_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut output = crate::runtime::tool_output::ToolOutput::structured(json!([{"kept":true}]))
        .with_effects(crate::tools::surface::outcome::Effects::Unknown)
        .with_changes([root.path().join("changed")]);
    post_tool(
        &[hook(
            "post_tool_use",
            None,
            "echo '{\"content\":\"replacement\",\"is_error\":false}'",
        )],
        root.path(),
        "edit",
        &json!({}),
        &mut output,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(output.text, "replacement");
    assert!(output.is_error && output.data.is_none());
    assert_eq!(
        output.execution.effects,
        crate::tools::surface::outcome::Effects::Unknown
    );
    assert_eq!(
        output.execution.changed_paths,
        [root.path().join("changed")]
    );
}

#[cfg(unix)]
fn hook(event: &str, matcher: Option<&str>, script: &str) -> HookConfig {
    HookConfig {
        event: event.into(),
        matcher: matcher.map(str::to_owned),
        command: vec!["sh".into(), "-c".into(), script.into()],
        timeout_secs: 5,
        fail_closed: None,
    }
}

#[cfg(windows)]
fn hook(event: &str, matcher: Option<&str>, script: &str) -> HookConfig {
    let script = match script {
        "grep -q '\"tool\":\"edit\"' && echo seen" => {
            "$text = [Console]::In.ReadToEnd(); if ($text.Contains('\"tool\":\"edit\"')) { 'seen' }"
        }
        "echo no >&2; exit 2" => "[Console]::Error.WriteLine('no'); exit 2",
        "sleep 5" => "Start-Sleep -Seconds 5",
        "exit 1" => "exit 1",
        "echo formatted" => "Write-Output 'formatted'",
        "echo lint failed >&2; exit 2" => "[Console]::Error.WriteLine('lint failed'); exit 2",
        "true" => "exit 0",
        r#"printf '{"input":{"command":"ls -la"},"notice":"normalized"}'"# => {
            r#"Write-Output '{"input":{"command":"ls -la"},"notice":"normalized"}'"#
        }
        r#"grep -q 'ls -la' || { echo 'rewrite not visible' >&2; exit 2; }"# => {
            "$text = [Console]::In.ReadToEnd(); if (!$text.Contains('ls -la')) { [Console]::Error.WriteLine('rewrite not visible'); exit 2 }"
        }
        r#"printf '{"decision":"block","reason":"policy"}'"# => {
            r#"Write-Output '{"decision":"block","reason":"policy"}'"#
        }
        r#"printf '{"content":"[redacted]","is_error":false}'"# => {
            r#"Write-Output '{"content":"[redacted]","is_error":false}'"#
        }
        _ => script,
    };
    HookConfig {
        event: event.into(),
        matcher: matcher.map(str::to_owned),
        command: vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        timeout_secs: 5,
        fail_closed: None,
    }
}

#[test]
fn exit_codes_map_to_verdicts_and_stdin_is_json() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = [hook(
        "pre_tool_use",
        Some("edit|write"),
        "grep -q '\"tool\":\"edit\"' && echo seen",
    )];
    let (verdict, notices) = fire(
        &hooks,
        dir.path(),
        "pre_tool_use",
        Some("edit"),
        json!({"tool":"edit"}),
        &AtomicBool::new(false),
    );
    assert_eq!(
        verdict,
        Verdict::Continue {
            output: "seen".into()
        }
    );
    assert!(notices.is_empty());
    let (verdict, _) = fire(
        &hooks,
        dir.path(),
        "pre_tool_use",
        Some("bash"),
        json!({"tool":"bash"}),
        &AtomicBool::new(false),
    );
    assert_eq!(
        verdict,
        Verdict::Continue {
            output: String::new()
        },
        "matcher is anchored and skips bash"
    );
    let hooks = [hook("pre_tool_use", None, "echo no >&2; exit 2")];
    assert_eq!(
        fire(
            &hooks,
            dir.path(),
            "pre_tool_use",
            Some("write"),
            json!({}),
            &AtomicBool::new(false),
        )
        .0,
        Verdict::Block {
            reason: "no".into()
        }
    );
}

#[test]
fn failures_and_timeouts_never_block() {
    let dir = tempfile::tempdir().unwrap();
    let mut slow = hook("stop", None, "sleep 5");
    slow.timeout_secs = 1;
    let hooks = [
        hook("stop", None, "exit 1"),
        slow,
        HookConfig {
            command: vec!["/no/such/program".into()],
            ..hook("stop", None, "")
        },
    ];
    let (verdict, notices) = fire(
        &hooks,
        dir.path(),
        "stop",
        None,
        json!({}),
        &AtomicBool::new(false),
    );
    assert!(matches!(verdict, Verdict::Continue { .. }));
    assert_eq!(notices.len(), 3, "{notices:?}");
}

#[test]
fn post_hook_output_reaches_the_model_and_block_marks_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut out = crate::runtime::tool_output::ToolOutput {
        execution: Default::default(),
        data: None,
        text: "ok".into(),
        is_error: false,
    };
    post_tool(
        &[hook("post_tool_use", None, "echo formatted")],
        dir.path(),
        "edit",
        &json!({}),
        &mut out,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(out.text, "ok\n[post_tool_use hook] formatted");
    assert!(!out.is_error);
    post_tool(
        &[hook("post_tool_use", None, "echo lint failed >&2; exit 2")],
        dir.path(),
        "edit",
        &json!({}),
        &mut out,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert!(out.is_error && out.text.ends_with("lint failed"));
}

#[test]
fn validation_rejects_bad_hooks() {
    assert!(validate(&hook("nope", None, "true")).is_err());
    assert!(validate(&hook("stop", Some("("), "true")).is_err());
    assert!(
        validate(&HookConfig {
            command: vec![],
            ..hook("stop", None, "")
        })
        .is_err()
    );
    assert!(validate(&hook("post_tool_use", Some("edit"), "true")).is_ok());
    assert!(
        validate(&hook("tool_call", None, "true")).is_ok(),
        "alias accepted"
    );
    assert!(validate(&hook("before_subagent_spawn", None, "true")).is_ok());
}

#[test]
fn crashing_fail_closed_pre_tool_hook_blocks_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let mut slow = hook("pre_tool_use", None, "sleep 5");
    slow.timeout_secs = 1;
    for failing in [
        hook("pre_tool_use", None, "exit 1"),
        slow,
        HookConfig {
            command: vec!["/no/such/program".into()],
            ..hook("pre_tool_use", None, "")
        },
    ] {
        assert!(
            fail_closed(&failing),
            "pre_tool_use defaults to fail-closed"
        );
        let result = pre_tool(
            &[failing],
            dir.path(),
            "bash",
            &json!({"command":"ls"}),
            &interrupt,
            &mut |_| {},
        );
        let reason = result.expect_err("a crashing policy hook must block");
        assert!(reason.contains("fail-closed"), "{reason}");
    }
    // Explicitly fail-open hooks only add a notice.
    let mut open = hook("pre_tool_use", None, "exit 1");
    open.fail_closed = Some(false);
    let mut notices = Vec::new();
    assert_eq!(
        pre_tool(
            &[open],
            dir.path(),
            "bash",
            &json!({}),
            &interrupt,
            &mut |n| notices.push(n.to_owned()),
        ),
        Ok(None)
    );
    assert_eq!(notices.len(), 1);
    // Other events stay fail-open by default; an opted-in stop hook blocks.
    assert!(!fail_closed(&hook("stop", None, "exit 1")));
    let mut strict = hook("stop", None, "exit 1");
    strict.fail_closed = Some(true);
    assert!(matches!(
        fire(&[strict], dir.path(), "stop", None, json!({}), &interrupt).0,
        Verdict::Block { .. }
    ));
}

#[test]
fn structured_output_rewrites_tool_input_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let rewrite = hook(
        "tool_call",
        Some("bash"),
        r#"printf '{"input":{"command":"ls -la"},"notice":"normalized"}'"#,
    );
    // The second hook observes the first hook's rewrite.
    let check = hook(
        "pre_tool_use",
        None,
        r#"grep -q 'ls -la' || { echo 'rewrite not visible' >&2; exit 2; }"#,
    );
    let mut notices = Vec::new();
    let rewritten = pre_tool(
        &[rewrite, check],
        dir.path(),
        "bash",
        &json!({"command":"ls"}),
        &interrupt,
        &mut |n| notices.push(n.to_owned()),
    )
    .unwrap();
    assert_eq!(rewritten, Some(json!({"command":"ls -la"})));
    assert_eq!(notices, ["hook: normalized"]);
    let blocked = pre_tool(
        &[hook(
            "pre_tool_use",
            None,
            r#"printf '{"decision":"block","reason":"policy"}'"#,
        )],
        dir.path(),
        "bash",
        &json!({}),
        &interrupt,
        &mut |_| {},
    );
    assert_eq!(
        blocked,
        Err("blocked by a pre_tool_use hook: policy".into())
    );
    let mut out = crate::runtime::tool_output::ToolOutput {
        execution: Default::default(),
        data: Some(json!({"raw":true})),
        text: "secret token=abc".into(),
        is_error: false,
    };
    post_tool(
        &[hook(
            "tool_result",
            None,
            r#"printf '{"content":"[redacted]","is_error":false}'"#,
        )],
        dir.path(),
        "bash",
        &json!({}),
        &mut out,
        &interrupt,
        &mut |_| {},
    );
    assert_eq!(out.text, "[redacted]");
    assert!(out.data.is_none() && !out.is_error);
}

#[test]
fn before_provider_request_rewrites_validates_and_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let request = crate::llm::Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let rewrite = hook(
        "before_provider_request",
        None,
        r#"printf '{"payload":{"messages":[{"role":"user","content":"rewritten"}],"tools":[],"temperature":0.0,"max_tokens":64,"seed":null}}'"#,
    );
    // The second hook sees the first hook's rewritten body in its payload.
    let check = hook(
        "before_provider_request",
        None,
        r#"grep -q rewritten || { echo 'rewrite not visible' >&2; exit 2; }"#,
    );
    let mut notices = Vec::new();
    let sent = provider_request(
        &[rewrite, check],
        dir.path(),
        &request,
        &interrupt,
        &mut |n| notices.push(n.to_owned()),
    )
    .unwrap()
    .expect("a hook rewrote the request");
    assert_eq!(
        sent.messages,
        vec![json!({"role":"user","content":"rewritten"})]
    );
    assert_eq!(sent.max_tokens, 64);
    assert!(notices.is_empty(), "{notices:?}");

    // A rewrite that is not a valid request is refused, never sent.
    let malformed = hook(
        "before_provider_request",
        None,
        r#"printf '{"payload":{"messages":"nope"}}'"#,
    );
    assert!(provider_request(&[malformed], dir.path(), &request, &interrupt, &mut |_| {}).is_err());

    // A block refuses the request and carries its reason.
    let blocked = hook(
        "before_provider_request",
        None,
        r#"printf '{"decision":"block","reason":"no egress"}'"#,
    );
    let error = provider_request(&[blocked], dir.path(), &request, &interrupt, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("no egress"), "{error}");

    // Without hooks the request is sent unchanged and nothing is cloned.
    assert!(
        provider_request(&[], dir.path(), &request, &interrupt, &mut |_| {})
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn oversized_json_reply_is_reported_not_read_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"printf '{"notice":"'; head -c 40000 /dev/zero | tr '\0' a; printf '"}'"#;
    let (verdict, notices) = fire(
        &[hook("stop", None, script)],
        dir.path(),
        "stop",
        None,
        json!({}),
        &AtomicBool::new(false),
    );
    assert!(matches!(verdict, Verdict::Continue { .. }));
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("exceeds"), "{notices:?}");
}

#[test]
fn matchers_are_compiled_once() {
    let h = hook("pre_tool_use", Some("edit|write"), "true");
    assert!(matches(&h, Some("edit")));
    assert!(!matches(&h, Some("editor")));
    assert!(MATCHERS.lock().contains_key("edit|write"));
}

#[test]
fn scheduling_ignores_hooks_that_match_other_tools() {
    let hooks = [hook("pre_tool_use", Some("bash"), "true")];
    assert!(wanted_for_tool(&hooks, "pre_tool_use", "bash"));
    assert!(!wanted_for_tool(&hooks, "pre_tool_use", "read"));
    assert!(!wanted_for_tool(&hooks, "post_tool_use", "bash"));
    let unmatched = [hook("tool_call", None, "true")];
    assert!(wanted_for_tool(&unmatched, "pre_tool_use", "read"));
}

fn chat(messages: Vec<Value>) -> crate::llm::Chat {
    crate::llm::Chat {
        messages,
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    }
}

#[test]
fn new_events_and_aliases_validate_and_canonicalize() {
    for (name, canonical) in [
        ("context", "context"),
        ("assistant_message", "assistant_message"),
        ("message_end", "assistant_message"),
        ("after_provider_response", "after_provider_response"),
        ("provider_response", "after_provider_response"),
        ("session_before_branch", "session_before_branch"),
        ("before_branch", "session_before_branch"),
        ("session_before_switch", "session_before_switch"),
        ("before_switch", "session_before_switch"),
    ] {
        assert!(validate(&hook(name, None, "true")).is_ok(), "{name}");
        assert_eq!(crate::config::hooks::canonical(name), canonical);
    }
}

#[cfg(unix)]
#[test]
fn context_hook_replaces_one_request_without_touching_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let original = chat(vec![json!({"role":"user","content":"hello"})]);
    let hooks = [
        hook(
            "context",
            None,
            r#"printf '{"messages":[{"role":"user","content":"rewritten"}],"additional_context":"mind the style guide"}'"#,
        ),
        // Later hooks see the earlier replacement.
        hook(
            "context",
            None,
            r#"grep -q rewritten || { echo 'replacement not visible' >&2; exit 2; }"#,
        ),
    ];
    let sent = provider_request(
        &hooks,
        dir.path(),
        &original,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap()
    .expect("the hook changed the request");
    assert_eq!(sent.messages[0]["content"], "rewritten");
    let note = sent.messages.last().unwrap();
    assert_eq!(note["content"], "mind the style guide");
    assert_eq!(note["dial_transient"], true);
    assert_eq!(original.messages.len(), 1);
    assert_eq!(original.messages[0]["content"], "hello");
}

#[cfg(unix)]
#[test]
fn context_hook_invalid_or_oversized_replacements_are_skipped_and_blocks_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let original = chat(vec![json!({"role":"user","content":"hello"})]);
    let big = "x".repeat(MAX_CONTEXT_MESSAGES + 1);
    for script in [
        r#"printf '{"messages":"nope"}'"#.to_owned(),
        r#"printf '{"messages":[{"content":"no role"}]}'"#.to_owned(),
        format!(r#"printf '{{"messages":[{{"role":"user","content":"{big}"}}]}}'"#),
    ] {
        let mut notices = Vec::new();
        let sent = provider_request(
            &[hook("context", None, &script)],
            dir.path(),
            &original,
            &AtomicBool::new(false),
            &mut |n| notices.push(n.to_owned()),
        );
        // The oversized stdout is rejected by the output cap before it can replace anything.
        assert!(sent.unwrap().is_none(), "{script:.60}");
    }
    let mut notices = Vec::new();
    provider_request(
        &[hook(
            "context",
            None,
            r#"printf '{"messages":[{"nope":1}]}'"#,
        )],
        dir.path(),
        &original,
        &AtomicBool::new(false),
        &mut |n| notices.push(n.to_owned()),
    )
    .unwrap();
    assert!(
        notices.iter().any(|n| n.contains("replacement ignored")),
        "{notices:?}"
    );
    let refused = provider_request(
        &[hook("context", None, "echo stale >&2; exit 2")],
        dir.path(),
        &original,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .expect_err("a context block refuses the request");
    assert!(refused.to_string().contains("stale"), "{refused}");
}

#[cfg(unix)]
#[test]
fn additional_context_is_bounded_deduplicated_and_dropped_by_a_block() {
    let dir = tempfile::tempdir().unwrap();
    let long = "y".repeat(MAX_CONTEXT * 2);
    let hooks = [
        hook(
            "pre_tool_use",
            None,
            r#"printf '{"additional_context":"  use the cache  "}'"#,
        ),
        hook(
            "pre_tool_use",
            None,
            r#"printf '{"additionalContext":"use the cache"}'"#,
        ),
        hook(
            "pre_tool_use",
            None,
            &format!(r#"printf '{{"additional_context":"{long}"}}'"#),
        ),
    ];
    let (result, context) = pre_tool_noted(
        &hooks,
        dir.path(),
        "bash",
        &json!({}),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(result, Ok(None));
    let context = context.expect("a note was queued");
    assert!(context.starts_with("use the cache\n"), "{context}");
    assert_eq!(context.matches("use the cache").count(), 1);
    assert!(context.len() <= MAX_OUTPUT + " [cut]".len());

    let hooks = [
        hook(
            "pre_tool_use",
            None,
            r#"printf '{"additional_context":"discard me"}'"#,
        ),
        hook("pre_tool_use", None, "echo no >&2; exit 2"),
    ];
    let (result, context) = pre_tool_noted(
        &hooks,
        dir.path(),
        "bash",
        &json!({}),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert!(result.is_err());
    assert_eq!(context, None, "a refusal discards earlier context");
}

#[cfg(unix)]
#[test]
fn post_tool_context_stays_outside_the_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut out = crate::runtime::tool_output::ToolOutput {
        execution: Default::default(),
        data: None,
        text: "ok".into(),
        is_error: true,
    };
    let context = post_tool_with(
        &[hook(
            "post_tool_use",
            None,
            r#"printf '{"additional_context":"retry with --force-with-lease"}'"#,
        )],
        dir.path(),
        "bash",
        &json!({}),
        &mut out,
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert_eq!(context.as_deref(), Some("retry with --force-with-lease"));
    assert_eq!(out.text, "ok", "the note never edits the tool result");
    assert!(out.is_error);
}

#[cfg(unix)]
#[test]
fn additional_context_is_ignored_on_events_that_do_not_deliver_it() {
    let dir = tempfile::tempdir().unwrap();
    let hooks = [hook(
        "stop",
        None,
        r#"printf '{"additional_context":"ignored"}'"#,
    )];
    let outcome = fire_with(
        &hooks,
        dir.path(),
        "stop",
        None,
        json!({}),
        &AtomicBool::new(false),
    );
    assert!(outcome.context_note().is_none());
}

#[cfg(unix)]
#[test]
fn assistant_message_hooks_get_the_reply_and_may_queue_context_but_never_block_it() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("seen.json");
    let hooks = [hook(
        "message_end",
        None,
        &format!(
            r#"cat > '{}'; printf '{{"additional_context":"cite sources next time"}}'"#,
            seen.display()
        ),
    )];
    let outcome = fire_with(
        &hooks,
        dir.path(),
        "assistant_message",
        None,
        json!({"text":"final answer"}),
        &AtomicBool::new(false),
    );
    assert_eq!(
        outcome.context_note().as_deref(),
        Some("cite sources next time")
    );
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(payload["event"], "assistant_message");
    assert_eq!(payload["text"], "final answer");
}

#[cfg(unix)]
#[test]
fn after_provider_response_reports_usage_and_cannot_block() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("response.json");
    let reply = crate::llm::Reply {
        finish_reason: "stop".into(),
        prompt_tokens: Some(120),
        completion_tokens: Some(8),
        ..Default::default()
    };
    // `exit 2` is a block everywhere else; an observer's refusal changes nothing here.
    let hooks = [hook(
        "after_provider_response",
        None,
        &format!("cat > '{}'; echo refused >&2; exit 2", seen.display()),
    )];
    let mut notices = Vec::new();
    provider_response(
        &hooks,
        dir.path(),
        "gpt-test",
        &reply,
        Duration::from_millis(42),
        &mut |n| notices.push(n.to_owned()),
    );
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(payload["model"], "gpt-test");
    assert_eq!(payload["stop_reason"], "stop");
    assert_eq!(payload["latency_ms"], 42);
    assert_eq!(payload["usage"]["prompt_tokens"], 120);
    assert_eq!(payload["usage"]["completion_tokens"], 8);
    assert!(notices.is_empty(), "{notices:?}");
}

#[cfg(unix)]
#[test]
fn session_change_gates_refuse_with_the_hook_reason_and_honor_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    for (configured, event) in [
        ("before_branch", "session_before_branch"),
        ("session_before_branch", "session_before_branch"),
        ("before_switch", "session_before_switch"),
    ] {
        let hooks = [hook(
            configured,
            None,
            "echo 'uncommitted work' >&2; exit 2",
        )];
        let refused = gate(
            &hooks,
            dir.path(),
            event,
            json!({"reason":"branch"}),
            &interrupt,
            &mut |_| {},
        );
        assert_eq!(refused, Err("uncommitted work".into()), "{configured}");
    }
    // A hook for the other event does not fire, and an allowing hook lets the change proceed.
    let hooks = [
        hook("session_before_switch", None, "exit 2"),
        hook("session_before_branch", None, "true"),
    ];
    assert_eq!(
        gate(
            &hooks,
            dir.path(),
            "session_before_branch",
            json!({}),
            &interrupt,
            &mut |_| {},
        ),
        Ok(())
    );
}

#[test]
fn user_prompt_submit_prompt_rewrite_reaches_later_hooks_and_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let rewrite = hook(
        "user_prompt_submit",
        None,
        r#"printf '{"prompt":"rewritten"}'"#,
    );
    // The second hook sees the rewritten prompt in its payload; a miss blocks the prompt.
    let check = hook(
        "user_prompt_submit",
        None,
        r#"grep -q rewritten || { echo 'rewrite not visible' >&2; exit 2; }"#,
    );
    let outcome = fire_with(
        &[rewrite, check],
        dir.path(),
        "user_prompt_submit",
        None,
        json!({"prompt":"original"}),
        &interrupt,
    );
    assert!(matches!(outcome.verdict, Verdict::Continue { .. }));
    assert_eq!(outcome.prompt.as_deref(), Some("rewritten"));
}

#[cfg(unix)]
#[test]
fn before_compact_notes_are_attributed_bounded_and_discarded_on_refusal() {
    let root = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let mut hooks = (0..4)
        .map(|i| {
            hook(
                "before_compact",
                None,
                &format!(
                    "printf '%s' '{}'",
                    json!({"additional_context":format!("{i}{}", "é".repeat(2300))})
                ),
            )
        })
        .collect::<Vec<_>>();
    let mut notices = Vec::new();
    let note = gate_noted(
        &hooks,
        root.path(),
        "before_compact",
        json!({}),
        &interrupt,
        &mut |text| notices.push(text.to_owned()),
    )
    .unwrap()
    .unwrap();
    assert!(note.contains("operator hook") && note.contains("untrusted data"));
    assert!(note.len() <= MAX_OUTPUT);
    assert!(note.contains("[cut]"));
    assert!(notices.iter().any(|text| text.contains("aggregate limit")));
    hooks.push(hook("before_compact", None, "echo refused >&2; exit 2"));
    assert_eq!(
        gate_noted(
            &hooks,
            root.path(),
            "before_compact",
            json!({}),
            &interrupt,
            &mut |_| {}
        ),
        Err("refused".into())
    );
    hooks.pop();
    let mut closed = hook("before_compact", None, "exit 1");
    closed.fail_closed = Some(true);
    hooks.push(closed);
    assert!(
        gate_noted(
            &hooks,
            root.path(),
            "before_compact",
            json!({}),
            &interrupt,
            &mut |_| {}
        )
        .unwrap_err()
        .contains("fail-closed")
    );
}
