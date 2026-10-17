use super::*;
#[cfg(unix)]
#[test]
fn presentation_hooks_cannot_clear_execution_uncertainty_or_committed_paths() {
    let root = tempfile::tempdir().unwrap();
    let mut output = crate::tools::ToolOutput::structured(json!([{"kept":true}]))
        .with_effects(crate::tools::outcome::Effects::Unknown)
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
        crate::tools::outcome::Effects::Unknown
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
    let mut out = crate::tools::ToolOutput {
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
    let mut out = crate::tools::ToolOutput {
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
        None,
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
    assert!(
        provider_request(
            &[malformed],
            None,
            dir.path(),
            &request,
            &interrupt,
            &mut |_| {}
        )
        .is_err()
    );

    // A block refuses the request and carries its reason.
    let blocked = hook(
        "before_provider_request",
        None,
        r#"printf '{"decision":"block","reason":"no egress"}'"#,
    );
    let error = provider_request(
        &[blocked],
        None,
        dir.path(),
        &request,
        &interrupt,
        &mut |_| {},
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("no egress"), "{error}");

    // Without hooks the request is sent unchanged and nothing is cloned.
    assert!(
        provider_request(&[], None, dir.path(), &request, &interrupt, &mut |_| {})
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
    assert!(wanted_for_tool(&hooks, None, "pre_tool_use", "bash"));
    assert!(!wanted_for_tool(&hooks, None, "pre_tool_use", "read"));
    assert!(!wanted_for_tool(&hooks, None, "post_tool_use", "bash"));
    let unmatched = [hook("tool_call", None, "true")];
    assert!(wanted_for_tool(&unmatched, None, "pre_tool_use", "read"));
}
