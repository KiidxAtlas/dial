use super::*;
use serde_json::json;

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "1".into(),
        name: name.into(),
        arguments: args,
        malformed_arguments: false,
    }
}

fn reply(text: &str, calls: Vec<ToolCall>) -> Reply {
    Reply {
        text: text.into(),
        tool_calls: calls,
        finish_reason: "length".into(),
        prompt_tokens: None,
        completion_tokens: None,
        ..Default::default()
    }
}

#[test]
fn equivalent_simple_shell_spellings_share_a_fingerprint() {
    let first = call(
        "bash",
        json!({"command":"cargo test --offline", "timeout":10}),
    );
    let second = call(
        "bash",
        json!({"command":"  cargo   'test'   --offline  ", "timeout":30}),
    );
    assert_eq!(normalized_arguments(&first), normalized_arguments(&second));
    assert_eq!(first.arguments["command"], "cargo test --offline");
    for command in ["echo *", "echo '$HOME'", "echo a && echo b", "echo \\*"] {
        let raw = call("bash", json!({"command":command}));
        assert_eq!(normalized_arguments(&raw)["command"], command);
    }
    let different = call("bash", json!({"command":"cargo test --offline other_test"}));
    assert_ne!(
        normalized_arguments(&first),
        normalized_arguments(&different)
    );
}

#[test]
fn paraphrased_verification_promises_get_feedback_without_a_wording_only_stop() {
    let mut guard = NarrationGuard::default();
    for (index, text) in [
        "I'll verify the result.",
        "Next I will run the check.",
        "Now I will test the change.",
    ]
    .iter()
    .enumerate()
    {
        let result = guard.observe(&reply(
            text,
            vec![call("read", json!({"path":index.to_string()}))],
        ));
        assert_eq!(result.is_some(), index == 2);
    }
    guard.progress();
    assert!(
        guard
            .observe(&reply(
                "I'll check a changed source.",
                vec![call("read", json!({"path":"new"}))]
            ))
            .is_none()
    );
}

#[test]
fn narration_recovery_recurs_until_real_progress() {
    let mut guard = NarrationGuard::default();
    let response = reply(
        "I'll verify the result.",
        vec![call("read", json!({"path":"a"}))],
    );
    for round in 1..=9 {
        assert_eq!(guard.observe(&response).is_some(), round % 3 == 0);
    }
    guard.progress();
    assert!(guard.observe(&response).is_none());
}

#[test]
fn cosmetic_arguments_do_not_disguise_failures_but_payloads_do() {
    let mut guard = RepeatGuard::default();
    for n in 0..FAILURE_LIMIT {
        let retry = call(
            "bash",
            json!({"command":"false", "timeout":n+1, "i":n.to_string(), "_intent":n.to_string()}),
        );
        let advice = guard.observe(&retry, true).unwrap_or_default();
        assert_eq!(advice.contains("Escalation"), n + 1 == FAILURE_LIMIT);
    }
    let original = call("write", json!({"path":"a", "content":{"i":"first"}}));
    let changed = call("write", json!({"path":"a", "content":{"i":"different"}}));
    assert_ne!(
        normalized_arguments(&original),
        normalized_arguments(&changed)
    );
    let changed = call("bash", json!({"command":"true"}));
    assert!(guard.observe(&changed, false).is_none());
}

#[test]
fn clipping_keeps_both_ends_and_reports_what_was_dropped() {
    let body: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    let out = clip_middle(&body, 4000);
    assert!(
        out.starts_with("line 0\n") && out.ends_with("line 1999\n"),
        "{out}"
    );
    assert!(
        out.contains("bytes omitted from the middle")
            && out.contains(&format!("of {} bytes", body.len()))
    );
    assert!(out.len() < 4300, "{}", out.len());
    assert_eq!(clip_middle("short", 4000), "short");
    // Multi-byte text is never split mid-character.
    let wide = "é".repeat(5000);
    assert!(clip_middle(&wide, 1001).is_char_boundary(0));
}

#[test]
fn evidence_clipping_keeps_middle_diagnostics_and_stays_bounded() {
    let noise = |n: usize| {
        (0..n)
            .map(|i| format!("   Compiling crate-{i} v0.1.0\n"))
            .collect::<String>()
    };
    let log = format!(
        "{}error[E0425]: cannot find value `x`\n  --> src/main.rs:3:5\n{}error: could not compile\n",
        noise(400),
        noise(400)
    );
    let out = clip_evidence(&log, 4000);
    assert!(
        out.contains("2 diagnostic line(s)")
            && out.contains("error[E0425]: cannot find value `x`\n  --> src/main.rs:3:5\n"),
        "{out}"
    );
    assert!(
        out.ends_with("error: could not compile\n") && out.len() < 4300,
        "{}",
        out.len()
    );
    // Without diagnostics it is plain head-and-tail clipping.
    let plain = noise(800);
    assert_eq!(clip_evidence(&plain, 4000), clip_middle(&plain, 4000));
    assert_eq!(clip_evidence("short", 4000), "short");
}

#[test]
fn the_same_failing_call_is_flagged_on_its_third_repeat_only() {
    let mut guard = RepeatGuard::default();
    let same = call(
        "edit",
        json!({"path": "a", "old_string": "x", "new_string": "y"}),
    );
    assert!(guard.observe(&same, true).is_none());
    assert!(guard.observe(&same, true).is_none());
    let advice = guard.observe(&same, true).expect("third identical failure");
    assert!(advice.contains("failed 3 times") && advice.contains("same arguments"));
    assert!(guard.observe(&same, true).unwrap().contains("4 times"));
    // Other calls do not erase a failure history; success for the exact call does.
    let changed = call(
        "edit",
        json!({"path": "a", "old_string": "x2", "new_string": "y"}),
    );
    assert!(guard.observe(&changed, true).is_none());
    assert!(guard.observe(&same, true).unwrap().contains("5 times"));
    assert!(guard.observe(&same, true).unwrap().contains("6 times"));
    assert!(guard.observe(&same, false).is_none());
    assert!(guard.observe(&same, true).is_none() && guard.observe(&same, true).is_none());
}

#[test]
fn identical_failures_escalate_at_each_multiple_without_ending_the_turn() {
    let mut guard = RepeatGuard::default();
    let same = call("bash", json!({"command":"false"}));
    let texts: Vec<String> = (0..12).filter_map(|_| guard.observe(&same, true)).collect();
    // Guidance starts at the third failure; escalation comes at 4, 8 and 12 failures.
    let escalated: Vec<usize> = texts
        .iter()
        .enumerate()
        .filter(|(_, text)| text.contains("Escalation"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!((texts.len(), escalated), (10, vec![1, 5, 9]));
    assert!(texts[9].contains("failed 12 times") && texts[9].contains("`bash`"));
}

#[test]
fn failed_reads_warn_immediately_and_allow_corrected_calls() {
    let mut guard = RepeatGuard::default();
    let malformed = call("read", json!({"path":"src/a.rs:25- fifty"}));
    assert!(
        guard
            .observe(&malformed, true)
            .unwrap()
            .contains("Read recovery")
    );
    // Failed reads get recovery guidance on every failure; they never escalate into a stop.
    let other = call("read", json!({"path":"src/b.rs:1-20"}));
    assert!(guard.observe(&other, false).is_none());
    assert!(
        guard
            .observe(&malformed, true)
            .unwrap()
            .contains("Read recovery")
    );
    let corrected = call("read", json!({"path":"src/a.rs:25-50"}));
    assert!(
        guard
            .observe(&corrected, true)
            .unwrap()
            .contains("Read recovery")
    );
    assert!(guard.observe(&malformed, false).is_none());
}

#[test]
fn repeated_narration_warns_periodically_despite_different_calls_and_whitespace() {
    let mut guard = NarrationGuard::default();
    let text = "I can see the current version. Let me verify the current state and check the UI header integration.";
    for n in 0..6 {
        let text = if n % 2 == 0 {
            text.to_owned()
        } else {
            text.replace(' ', "\n")
        };
        let r = reply(
            &text,
            vec![call("read", json!({"path":format!("source-{n}")}))],
        );
        assert_eq!(guard.observe(&r).is_some(), n % 3 == 2);
    }
    assert!(guard.observe(&reply(text, vec![])).is_none());
    assert!(
        guard
            .observe(&reply("Checking.", vec![call("read", json!({}))]))
            .is_none()
    );
    for n in 0..100 {
        let r = reply(
            &format!("{text} Unique explanation {n}."),
            vec![call("read", json!({}))],
        );
        assert!(guard.observe(&r).is_none());
    }
    assert_eq!(guard.seen.len(), 32);
}

#[test]
fn a_length_stop_first_asks_for_a_bigger_budget_that_fits_the_context() {
    let r = reply("", vec![]);
    assert_eq!(
        recover_from_length(4096, 32768, 2000, &r, 0),
        LengthAction::Retry { max_tokens: 8192 }
    );
    assert_eq!(
        recover_from_length(8192, 32768, 2000, &r, 0),
        LengthAction::Retry { max_tokens: 16384 }
    );
    // Never more than the window can spare after the prompt.
    assert_eq!(
        recover_from_length(8192, 32768, 20000, &r, 0),
        LengthAction::Retry { max_tokens: 11744 }
    );
    assert_eq!(
        recover_from_length(20000, 65536, 100, &r, 0),
        LengthAction::Retry { max_tokens: 32768 }
    );
}

#[test]
fn without_room_to_grow_the_model_is_nudged_then_escalated_never_ended() {
    let spent = recover_from_length(8192, 8192, 6000, &reply("", vec![]), 0);
    assert!(
        matches!(&spent, LengthAction::Nudge { partial: None, message } if message.contains("Think less") && !message.contains("Escalation")),
        "{spent:?}"
    );
    let partial = recover_from_length(8192, 8192, 6000, &reply("The answer is", vec![]), 0);
    assert!(
        matches!(&partial, LengthAction::Nudge { partial: Some(p), .. } if p == "The answer is"),
        "{partial:?}"
    );
    let cut_call = recover_from_length(
        8192,
        8192,
        6000,
        &reply("", vec![call("write", json!({}))]),
        1,
    );
    assert!(
        matches!(&cut_call, LengthAction::Nudge { partial: None, message } if message.contains("smaller calls")),
        "{cut_call:?}"
    );
    match recover_from_length(8192, 8192, 6000, &reply("", vec![]), MAX_LENGTH_NUDGES) {
        LengthAction::Nudge { message, .. } => {
            assert!(
                message.contains("Escalation")
                    && message.contains("8192 tokens")
                    && message.contains("Think less")
            )
        }
        other => panic!("{other:?}"),
    }
}
#[test]
fn zero_step_budget_is_unlimited() {
    let mut budget = StepBudget::new(0, 0);
    for _ in 0..100 {
        assert!(budget.begin());
    }
}

#[test]
fn unusable_replies_are_refunded_a_bounded_number_of_times() {
    let mut budget = StepBudget::new(3, 2);
    assert!(
        budget.begin() && budget.refund(),
        "a cut-off reply costs nothing"
    );
    assert!(budget.begin() && budget.refund());
    assert!(
        budget.begin() && !budget.refund(),
        "only two free retries, then it counts"
    );
    assert!(
        budget.begin() && budget.begin() && !budget.begin(),
        "three real steps were all it had"
    );
    // Evaluation runs grant no refunds: their step budget is the thing being measured.
    let mut measured = StepBudget::new(2, 0);
    assert!(measured.begin() && !measured.refund() && measured.begin() && !measured.begin());
}

#[test]
fn zero_step_budget_is_unlimited_but_refunds_remain_bounded() {
    let mut budget = StepBudget::new(0, 2);
    for _ in 0..100 {
        assert!(budget.begin());
    }
    assert!(budget.refund() && budget.refund() && !budget.refund());
    budget.spent = u32::MAX;
    assert!(budget.begin());
    assert_eq!(budget.spent, u32::MAX);
    assert!(budget.begin());
    let h = crate::config::types::Harness {
        max_steps: 0,
        ..Default::default()
    };
    assert_eq!(h.capped_steps(24), 24);
    assert_eq!(h.capped_steps(8), 8);
    assert_eq!(
        crate::config::types::Harness { max_steps: 2, ..h }.capped_steps(24),
        2
    );
}

#[test]
fn a_tool_that_keeps_failing_with_different_arguments_is_warned_then_escalated() {
    let mut guard = StallGuard::default();
    let edit = |n: u32| {
        call(
            "edit",
            json!({"path": "a.rs", "old_string": format!("guess {n}"), "new_string": "x"}),
        )
    };
    for n in 1..5 {
        assert!(
            matches!(guard.call(&edit(n), true), Stall::None),
            "failure {n}"
        );
    }
    let Stall::Warn(advice) = guard.call(&edit(5), true) else {
        panic!("warns at the fifth")
    };
    assert!(
        advice.contains("`edit`")
            && advice.contains("5 times in a row")
            && advice.contains("different arguments")
    );
    for n in 6..10 {
        assert!(
            matches!(guard.call(&edit(n), true), Stall::None),
            "warns once, not every time"
        );
    }
    let Stall::Warn(text) = guard.call(&edit(10), true) else {
        panic!("escalates at the tenth")
    };
    assert!(text.contains("`edit`") && text.contains("10 times") && text.contains("Escalation"));
}

#[test]
fn escalation_text_names_the_last_error_of_the_failing_tool() {
    let mut guard = StallGuard::default();
    let edit = |n: u32| {
        call(
            "edit",
            json!({"path": "a.rs", "old_string": format!("guess {n}"), "new_string": "x"}),
        )
    };
    for n in 1..10 {
        guard.note_error(
            "edit",
            &format!("\n  old_string not found in a.rs\nattempt {n}"),
        );
        guard.call(&edit(n), true);
    }
    guard.note_error("edit", "old_string not found in a.rs\nattempt 10");
    let Stall::Warn(text) = guard.call(&edit(10), true) else {
        panic!("escalates at the tenth")
    };
    assert!(
        text.contains("`edit`") && text.contains("Last error: old_string not found in a.rs."),
        "{text}"
    );
    assert!(
        !text.contains("attempt"),
        "only the first line is kept: {text}"
    );
}

#[test]
fn a_success_of_that_tool_resets_it_and_other_tools_do_not() {
    let mut guard = StallGuard::default();
    let edit = || {
        call(
            "edit",
            json!({"path": "a", "old_string": "x", "new_string": "y"}),
        )
    };
    for _ in 0..4 {
        assert!(matches!(guard.call(&edit(), true), Stall::None));
    }
    // Reading a file in between does not clear an edit streak: that is the read-guess-read-guess loop.
    assert!(matches!(
        guard.call(&call("read", json!({"path": "a"})), false),
        Stall::None
    ));
    assert!(matches!(guard.call(&edit(), true), Stall::Warn(_)));
    assert!(matches!(guard.call(&edit(), false), Stall::None));
    for _ in 0..4 {
        assert!(
            matches!(guard.call(&edit(), true), Stall::None),
            "a fresh streak"
        );
    }
}

#[test]
fn a_failing_shell_command_is_task_feedback_not_a_stalled_call() {
    let mut guard = StallGuard::default();
    for n in 0..30 {
        assert!(matches!(
            guard.call(
                &call("bash", json!({"command": format!("cargo test {n}")})),
                true
            ),
            Stall::None
        ));
    }
}

#[test]
fn rounds_where_every_call_failed_are_counted_and_any_success_clears_them() {
    let mut guard = StallGuard::default();
    for round in 1..4 {
        assert!(matches!(guard.round(true), Stall::None), "round {round}");
    }
    let Stall::Warn(advice) = guard.round(true) else {
        panic!("warns at the fourth")
    };
    assert!(advice.contains("4 rounds"));
    assert!(matches!(guard.round(false), Stall::None));
    for _ in 0..7 {
        assert!(matches!(guard.round(true), Stall::None | Stall::Warn(_)));
    }
    assert!(
        matches!(guard.round(true), Stall::Warn(_)),
        "eight in a row"
    );
}

fn ok_output() -> crate::tools::ToolOutput {
    crate::tools::ToolOutput::ok("done".into())
}

fn shell(command: &str) -> ToolCall {
    call("bash", json!({ "command": command }))
}

#[test]
fn a_rerun_of_an_unchanged_command_is_pointed_at_its_earlier_result() {
    let mut pace = PaceGuard::default();
    assert!(
        pace.call(&shell("make lint 2>&1 | tail -15"), &ok_output())
            .is_none()
    );
    // Reading files changes nothing, so the first result is still current.
    assert!(
        pace.call(&call("read", json!({"path": "Makefile"})), &ok_output())
            .is_none()
    );
    let advice = pace
        .call(&shell("make lint 2>&1 | tail -15"), &ok_output())
        .expect("unchanged rerun is flagged");
    assert!(advice.contains("make lint"), "{advice}");
}

#[test]
fn a_rerun_after_the_project_may_have_changed_is_not_flagged() {
    let mut pace = PaceGuard::default();
    pace.call(&shell("cargo clippy --all-targets"), &ok_output());
    pace.call(&call("edit", json!({})), &ok_output());
    assert!(
        pace.call(&shell("cargo clippy --all-targets"), &ok_output())
            .is_none()
    );
    pace.call(&shell("cargo fmt"), &ok_output());
    assert!(
        pace.call(&shell("cargo clippy --all-targets"), &ok_output())
            .is_none(),
        "a command that may have written files resets what counts as current"
    );
}

#[test]
fn only_complete_runs_of_scannable_commands_are_remembered() {
    let mut pace = PaceGuard::default();
    let timed_out = ok_output().with_status(crate::tools::ToolStatus::Timeout);
    pace.call(&shell("cargo test"), &timed_out);
    assert!(pace.call(&shell("cargo test"), &ok_output()).is_none());
    // Substitution cannot be scanned, so it is never matched.
    pace.call(&shell("echo $(date)"), &ok_output());
    assert!(pace.call(&shell("echo $(date)"), &ok_output()).is_none());
}

#[test]
fn a_long_run_of_single_lookups_is_told_once_per_streak_to_batch() {
    let mut pace = PaceGuard::default();
    let lookup = [call("read", json!({"path": "a"}))];
    for round in 1..SERIAL_WARN {
        assert!(pace.round(&lookup).is_none(), "round {round}");
    }
    assert!(pace.round(&lookup).unwrap().contains("one response"));
    assert!(pace.round(&lookup).is_none());
    // A batch or a change of kind starts the count over.
    let batch = [lookup[0].clone(), call("grep", json!({"pattern": "x"}))];
    assert!(pace.round(&batch).is_none());
    for _ in 1..SERIAL_WARN {
        assert!(pace.round(&lookup).is_none());
    }
    assert!(pace.round(&lookup).is_some());
    let write = [call("edit", json!({}))];
    for _ in 0..SERIAL_WARN * 2 {
        assert!(pace.round(&write).is_none());
    }
}
