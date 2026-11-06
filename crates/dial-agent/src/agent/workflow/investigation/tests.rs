use super::*;
use serde_json::json;

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "first-read".into(),
        name: name.into(),
        arguments: args,
        malformed_arguments: false,
    }
}

#[test]
fn unchanged_reads_redirect_but_changed_results_and_mutations_do_not() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"src/main.rs", "i":"inspect"}));
    assert!(guard.observe(&read, "old", false).is_none());
    let again = call("read", json!({"path":"src/main.rs", "i":"inspect again"}));
    assert!(
        guard
            .observe(&again, "old", false)
            .unwrap()
            .contains("first-read")
    );
    assert!(guard.observe(&again, "new", false).is_none());
    assert!(guard.observe(&again, "new", true).is_none());
    guard.observe_output(
        &call("edit", json!({})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["changed.rs".into()]),
    );
    assert!(guard.observe(&again, "new", false).is_none());
}

#[test]
fn repeated_successful_shell_checks_escalate_even_when_timeout_and_artifact_change() {
    let mut guard = InvestigationGuard::default();
    let command =
        "cd project && grep '^version' pyproject.toml && grep '__version__' src/app/__init__.py";
    for attempt in 0..5 {
        let check = call("bash", json!({"command":command, "timeout":30+attempt}));
        let text = format!(
            "version = \"0.3.31\"\n__version__ = \"0.3.31\"\n[exit code: 0]\n[complete output: artifact://{attempt}]\n"
        );
        assert_eq!(guard.observe(&check, &text, false).is_some(), attempt > 0);
        assert_eq!(guard.escalation().is_some(), attempt == 4);
    }
}

#[test]
fn shell_checks_allow_new_results_different_commands_and_verification_after_edits() {
    let mut guard = InvestigationGuard::default();
    let check = call("bash", json!({"command":"cat version.txt"}));
    let old = "0.3.31\n[exit code: 0]\n";
    assert!(guard.observe(&check, old, false).is_none());
    assert!(guard.observe(&check, old, true).is_none());
    assert!(guard.observe(&check, old, false).is_some());
    assert!(
        guard
            .observe(&check, "0.3.32\n[exit code: 0]\n", false)
            .is_none()
    );
    assert!(
        guard
            .observe(
                &call("bash", json!({"command":"cat other-version.txt"})),
                old,
                false
            )
            .is_none()
    );
    guard.observe_output(
        &call("edit", json!({"path":"version.txt"})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["version.txt".into()]),
    );
    assert!(guard.observe(&check, old, false).is_none());
}

#[test]
fn independent_ranges_and_recovery_are_allowed_and_storage_is_bounded() {
    let mut guard = InvestigationGuard::default();
    for line in 0..200 {
        assert!(
            guard
                .observe(
                    &call("read", json!({"path":"x", "offset":line})),
                    "same",
                    false
                )
                .is_none()
        );
    }
    assert_eq!(guard.observations.len(), 128);
    for _ in 0..3 {
        assert!(
            guard
                .observe(
                    &call(
                        "read",
                        json!({"path":format!("artifact://{}", "a".repeat(64))})
                    ),
                    "same",
                    false
                )
                .is_none()
        );
    }
}

#[test]
fn repeated_evidence_escalates_but_new_evidence_rearms() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"a.rs"}));
    guard.observe(&read, "original", false);
    for _ in 0..3 {
        guard.observe(&read, "original", false);
        assert!(guard.escalation().is_none());
        guard.observe(&call("todo", json!({"op":"view"})), "plan", false);
    }
    guard.observe(&read, "original", false);
    assert!(guard.escalation().is_some());
    guard.observe(&read, "changed externally", false);
    assert!(guard.escalation().is_none());
}

#[test]
fn intervening_checks_errors_and_other_reads_do_not_hide_a_loop() {
    let mut guard = InvestigationGuard::default();
    let read = call("read", json!({"path":"src/steering.rs"}));
    guard.observe(&read, "pub(crate) fn local_unsupported_reason", false);
    for attempt in 0..4 {
        guard.observe(
            &call("bash", json!({"command":"cargo check"})),
            "subagents.rs: compile error",
            true,
        );
        guard.observe(
            &call("bash", json!({"command":"git status"})),
            "unchanged",
            false,
        );
        guard.observe(
            &call("read", json!({"path":format!("other-{attempt}.rs")})),
            "other evidence",
            false,
        );
        assert!(
            guard
                .observe(&read, "pub(crate) fn local_unsupported_reason", false)
                .is_some()
        );
    }
    assert!(guard.escalation().is_some());
    assert!(guard.observe(&read, "changed externally", false).is_none());
    assert!(guard.escalation().is_none());
}

#[test]
fn equivalent_anchored_reads_share_evidence_but_new_ranges_and_snapshots_do_not() {
    let mut guard = InvestigationGuard::default();
    let original = call("read", json!({"path":"a.rs:1-2"}));
    let text = "[a.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n";
    assert!(guard.observe(&original, text, false).is_none());
    for args in [
        json!({"path":"a.rs", "offset":1, "limit":2}),
        json!({"path":"a.rs:1+2"}),
        json!({"path":"./a.rs:1-2"}),
        json!({"path":"a.rs:1-2", "limit":50}),
    ] {
        assert!(guard.observe(&call("read", args), text, false).is_some());
    }
    assert!(guard.escalation().is_some());
    assert!(
        guard
            .observe(
                &call("read", json!({"path":"a.rs", "offset":1, "limit":2})),
                "[a.rs#AB12]\n1:one\n2:two\n",
                false
            )
            .is_some()
    );
    assert!(
        guard
            .observe(&original, "[a.rs#AB12]\n3:three\n[lines 3 of 3]\n", false)
            .is_none()
    );
    assert!(guard.escalation().is_none());
    assert!(
        guard
            .observe(
                &original,
                "[a.rs#CD34]\n1:changed\n2:two\n[lines 1-2 of 3]\n",
                false
            )
            .is_none()
    );
    assert!(
        guard
            .observe(
                &call("read", json!({"path":"b.rs:1-2"})),
                "[b.rs#AB12]\n1:one\n2:two\n[lines 1-2 of 3]\n",
                false
            )
            .is_none()
    );
}

#[test]
fn no_op_edits_and_todo_polling_do_not_erase_version_check_loops() {
    let mut guard = InvestigationGuard::default();
    let check = call("bash", json!({"command":"grep '^version' pyproject.toml"}));
    for attempt in 0..5 {
        assert_eq!(
            guard.observe(&check, "version = 0.3.31", false).is_some(),
            attempt > 0
        );
        guard.observe(&call("todo", json!({"op":"view"})), "unfinished", false);
        guard.observe(
            &call("edit", json!({})),
            "No changes: the edit leaves the file identical. Nothing was written.",
            false,
        );
        guard.observe(
            &call("write", json!({})),
            "change preview (+0 -0 lines)",
            false,
        );
        assert_eq!(guard.escalation().is_some(), attempt == 4);
    }
    guard.observe_output(
        &call("edit", json!({})),
        &crate::runtime::tool_output::ToolOutput::structured(json!({}))
            .with_changes(["changed.rs".into()]),
    );
    assert!(guard.escalation().is_none());
    assert!(guard.observe(&check, "version = 0.3.31", false).is_none());
}

#[test]
fn unchanged_read_gets_pointer_only_while_earlier_result_is_full() {
    let mut returned = ReturnedReads::default();
    let read = call("read", json!({"path":"src/lib.rs"}));
    let body = "1: fn a() {}\n".repeat(32);
    assert!(returned.pointer(&read, &body, false, |_| true).is_none());
    assert!(returned.pointer(&read, &body, false, |_| false).is_none());
    assert!(returned.pointer(&read, &body, false, |_| true).is_some());
    assert!(
        returned
            .pointer(&read, &"1: fn b() {}\n".repeat(32), false, |_| true)
            .is_none()
    );
}

fn rows(tag: &str, lines: std::ops::RangeInclusive<u32>) -> String {
    let mut text = format!("[src/lib.rs#{tag}]\n[Source raw SHA256:00]\n");
    for line in lines {
        text.push_str(&format!("{line}: code {line}\n"));
    }
    text
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        ..call("read", json!({ "path": path }))
    }
}

#[test]
fn a_read_inside_lines_already_shown_gets_a_pointer_to_that_call() {
    let mut returned = ReturnedReads::default();
    let whole = read_call("whole", "src/lib.rs");
    assert!(
        returned
            .pointer(&whole, &rows("AAAA", 1..=100), false, |_| true)
            .is_none()
    );
    let part = read_call("part", "src/lib.rs:40-60");
    let pointer = returned
        .pointer(&part, &rows("AAAA", 40..=60), false, |_| true)
        .expect("contained read is answered by a pointer");
    assert!(is_unchanged_read(&pointer) && pointer.contains("whole"));
}

#[test]
fn a_read_is_sent_in_full_when_it_adds_lines_the_file_changed_or_the_earlier_result_is_gone() {
    let mut returned = ReturnedReads::default();
    let whole = read_call("whole", "src/lib.rs:1-100");
    returned.pointer(&whole, &rows("AAAA", 1..=100), false, |_| true);
    let send = |returned: &mut ReturnedReads, id, text: String, full: bool| {
        returned.pointer(&read_call(id, id), &text, false, |_| full)
    };
    // One line past the earlier result.
    assert!(send(&mut returned, "past", rows("AAAA", 90..=101), true).is_none());
    // Same lines but the file changed since (new tag).
    assert!(send(&mut returned, "edited", rows("BBBB", 40..=60), true).is_none());
    // The earlier result is no longer shown in full.
    assert!(send(&mut returned, "pruned", rows("AAAA", 40..=60), false).is_none());
    // A result with no source rows is never replaced.
    assert!(send(&mut returned, "raw", "plain text".into(), true).is_none());
}

#[test]
fn multi_path_reads_are_replaced_only_when_every_section_is_covered() {
    let mut returned = ReturnedReads::default();
    let first = read_call("first", "a");
    let both = format!(
        "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
        rows("AAAA", 1..=50)
    );
    assert!(returned.pointer(&first, &both, false, |_| true).is_none());
    let again = format!(
        "{}[src/main.rs#CCCC]\n1: fn main() {{}}\n",
        rows("AAAA", 5..=9)
    );
    assert!(
        returned
            .pointer(&read_call("second", "b"), &again, false, |_| true)
            .is_some()
    );
    let extra = format!("{}[src/new.rs#DDDD]\n1: new\n", rows("AAAA", 5..=9));
    assert!(
        returned
            .pointer(&read_call("third", "c"), &extra, false, |_| true)
            .is_none()
    );
}

#[test]
fn adjacent_visible_reads_jointly_cover_a_later_range_without_hiding_gaps() {
    for (right_start, right_visible, covered) in
        [(11, true, true), (12, true, false), (11, false, false)]
    {
        let mut returned = ReturnedReads::default();
        returned.pointer(
            &read_call("left", "src/lib.rs:1-10"),
            &rows("AAAA", 1..=10),
            false,
            |_| true,
        );
        returned.pointer(
            &read_call("right", "src/lib.rs:11-20"),
            &rows("AAAA", right_start..=20),
            false,
            |_| true,
        );
        let pointer = returned.pointer(
            &read_call("combined", "src/lib.rs:5-15"),
            &rows("AAAA", 5..=15),
            false,
            |id| id == "left" || right_visible,
        );
        assert_eq!(pointer.is_some(), covered);
        if let Some(pointer) = pointer {
            assert!(pointer.contains("left") && pointer.contains("right"));
        }
    }
}

#[test]
fn visible_edit_and_discovery_rows_cover_rereads_but_not_unseen_or_stale_lines() {
    for name in ["edit", "write", "glob", "find", "grep"] {
        let mut returned = ReturnedReads::default();
        let source = ToolCall {
            id: "source-result".into(),
            ..call(name, json!({}))
        };
        let text = rows("AAAA", 5..=12);
        assert!(returned.pointer(&source, &text, false, |_| true).is_none());
        let pointer = returned.pointer(
            &read_call("again", "src/lib.rs:6-11"),
            &rows("AAAA", 6..=11),
            false,
            |_| true,
        );
        assert!(
            pointer.is_some_and(|text| text.contains("source-result")),
            "{name}"
        );
        for (tag, range, visible) in [
            ("AAAA", 4..=8, true),
            ("BBBB", 6..=7, true),
            ("AAAA", 6..=7, false),
        ] {
            assert!(
                returned
                    .pointer(
                        &read_call("fresh", "src/lib.rs"),
                        &rows(tag, range),
                        false,
                        |_| visible
                    )
                    .is_none(),
                "{name}"
            );
        }
    }
}

#[test]
fn errors_shell_output_and_truncated_rows_do_not_seed_read_reuse() {
    for (name, error, text) in [
        ("edit", true, rows("AAAA", 1..=2)),
        ("bash", false, rows("AAAA", 1..=2)),
        (
            "glob",
            false,
            "[src/lib.rs#AAAA]\n1:partial …[truncated]\n".into(),
        ),
    ] {
        let mut returned = ReturnedReads::default();
        returned.pointer(&call(name, json!({})), &text, error, |_| true);
        assert!(
            returned
                .pointer(
                    &read_call("fresh", "src/lib.rs"),
                    &rows("AAAA", 1..=1).replace("code 1", &"source".repeat(100)),
                    false,
                    |_| true
                )
                .is_none(),
            "{name}"
        );
    }
}

#[test]
fn source_reuse_pointer_is_smaller_and_does_not_replace_tiny_ranges() {
    let mut returned = ReturnedReads::default();
    returned.pointer(
        &call("edit", json!({})),
        &rows("AAAA", 1..=20),
        false,
        |_| true,
    );
    let full = rows("AAAA", 5..=15);
    let pointer = returned
        .pointer(&read_call("part", "src/lib.rs:5-15"), &full, false, |_| {
            true
        })
        .unwrap();
    assert!(pointer.len() < full.len());
    assert!(
        returned
            .pointer(
                &read_call("tiny", "src/lib.rs:1"),
                &rows("AAAA", 1..=1),
                false,
                |_| true
            )
            .is_none()
    );
}

#[test]
fn identical_reads_never_expand_tiny_or_equal_sized_results() {
    let read = read_call("first", "src/lib.rs");
    let expected = "[Unchanged read] Fresh read succeeded; same rows in still-visible tool call first. Reuse them.";
    for text in ["x".into(), rows("AAAA", 1..=1), "x".repeat(expected.len())] {
        let mut returned = ReturnedReads::default();
        for _ in 0..3 {
            assert!(returned.pointer(&read, &text, false, |_| true).is_none());
        }
    }
}

#[test]
fn identical_reads_use_the_compact_pointer_only_when_it_saves_bytes() {
    let read = read_call("first", "src/lib.rs");
    let mut returned = ReturnedReads::default();
    let text = rows("AAAA", 1..=20);
    assert!(returned.pointer(&read, &text, false, |_| true).is_none());
    let pointer = returned.pointer(&read, &text, false, |_| true).unwrap();
    assert!(pointer.len() < text.len());
    assert_eq!(
        pointer,
        "[Unchanged read] Fresh read succeeded; same rows in still-visible tool call first. Reuse them."
    );
    assert!(returned.pointer(&read, &text, false, |_| false).is_none());
}

fn native_fixture() -> (tempfile::TempDir, crate::tools::Workspace, ReturnedReads) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
        root.path().join("src/lib.rs"),
        (1..=30).map(|n| format!("code {n}\n")).collect::<String>(),
    )
    .unwrap();
    let workspace = crate::tools::Workspace::new(root.path()).unwrap();
    (root, workspace, ReturnedReads::default())
}

fn remember_native(
    returned: &mut ReturnedReads,
    workspace: &crate::tools::Workspace,
    id: &str,
    path: &str,
) {
    let call = read_call(id, path);
    let output = crate::tools::execute(workspace, "read", &call.arguments, 10);
    assert!(!output.is_error, "{}", output.text);
    returned.pointer(&call, &output.text, false, |_| true);
}

#[test]
fn native_coverage_reuses_complete_ranges_and_executes_only_missing_gaps() {
    let (root, workspace, mut returned) = native_fixture();
    // Realistically sized rows: a pointer only replaces rows it is actually smaller than.
    std::fs::write(
        root.path().join("src/lib.rs"),
        (1..=30)
            .map(|n| format!("code {n} {}\n", "x".repeat(40)))
            .collect::<String>(),
    )
    .unwrap();
    remember_native(&mut returned, &workspace, "left", "src/lib.rs:1-10");
    remember_native(&mut returned, &workspace, "right", "src/lib.rs:21-30");
    let reuse = returned.reusable(|_| true);
    let mut invoked = Vec::new();
    let full = reuse.execute(&workspace, &json!({"path":"src/lib.rs:2-8"}), |_| {
        panic!("fully covered range must not execute a native read")
    });
    assert!(is_unchanged_read(&full.text));
    assert_eq!(
        full.data.as_ref().unwrap()["read_reuse"]["fully_reused"],
        true
    );
    let partial = reuse.execute(&workspace, &json!({"path":"src/lib.rs:5-25"}), |args| {
        invoked.push(args.clone());
        crate::tools::execute(&workspace, "read", args, 10)
    });
    assert!(!partial.is_error, "{}", partial.text);
    assert_eq!(invoked, [json!({"path":"src/lib.rs:11-20"})]);
    let coverage = &partial.data.as_ref().unwrap()["read_reuse"];
    assert_eq!(coverage["executed_ranges"], json!([[11, 20]]));
    assert_eq!(
        coverage["covered"],
        json!([
            {"range":[5,10],"call_id":"left"}, {"range":[21,25],"call_id":"right"}
        ])
    );
    assert!(partial.text.contains("11:code 11") && !partial.text.contains("5:code 5"));
}

#[test]
fn native_coverage_reads_tiny_rows_instead_of_a_larger_pointer() {
    let (_root, workspace, mut returned) = native_fixture();
    remember_native(&mut returned, &workspace, "left", "src/lib.rs:1-10");
    let reuse = returned.reusable(|_| true);
    let mut invoked = Vec::new();
    let output = reuse.execute(&workspace, &json!({"path":"src/lib.rs:2-3"}), |args| {
        invoked.push(args.clone());
        crate::tools::execute(&workspace, "read", args, 10)
    });
    assert_eq!(invoked, [json!({"path":"src/lib.rs:2-3"})]);
    assert!(output.text.contains("2:code 2"), "{}", output.text);
    assert!(!is_unchanged_read(&output.text));
}

#[test]
fn native_coverage_invalidates_on_edits_missing_context_and_unsupported_representations() {
    let (root, workspace, mut returned) = native_fixture();
    remember_native(&mut returned, &workspace, "original", "src/lib.rs:1-30");
    let args = json!({"path":"src/lib.rs:1-10"});
    let mut calls = 0;
    returned
        .reusable(|_| false)
        .execute(&workspace, &args, |effective| {
            calls += 1;
            assert_eq!(effective, &args);
            crate::tools::execute(&workspace, "read", effective, 10)
        });
    assert_eq!(calls, 1, "pruned evidence must execute again");
    let reuse = returned.reusable(|_| true);
    for args in [
        json!({"path":"src/lib.rs:raw:1-10"}),
        json!({"paths":["src/lib.rs:1-10"]}),
        json!({"path":"src/lib.rs:outline"}),
    ] {
        let mut calls = 0;
        reuse.execute(&workspace, &args, |effective| {
            calls += 1;
            assert_eq!(effective, &args);
            crate::runtime::tool_output::ToolOutput::ok("normal representation".into())
        });
        assert_eq!(calls, 1);
    }
    std::fs::write(root.path().join("src/lib.rs"), "changed\n").unwrap();
    let output = reuse.execute(&workspace, &args, |effective| {
        calls += 1;
        assert_eq!(effective, &args);
        crate::tools::execute(&workspace, "read", effective, 10)
    });
    assert_eq!(calls, 2, "changed source must execute the requested read");
    assert!(output.text.contains("changed"));
    std::fs::remove_file(root.path().join("src/lib.rs")).unwrap();
    let output = reuse.execute(&workspace, &args, |effective| {
        crate::tools::execute(&workspace, "read", effective, 10)
    });
    assert!(
        output.is_error,
        "disappearance must not return stale evidence"
    );
}

#[test]
fn native_coverage_does_not_cover_clipped_rows_and_reconciles_a_racing_edit() {
    let (root, workspace, mut returned) = native_fixture();
    let call = read_call("clipped", "src/lib.rs:1-10");
    let output = crate::tools::execute(&workspace, "read", &call.arguments, 10);
    let clipped = output
        .text
        .replace("5:code 5", "5:code 5 …[line truncated]");
    returned.pointer(&call, &clipped, false, |_| true);
    let reuse = returned.reusable(|_| true);
    let mut invoked = Vec::new();
    let output = reuse.execute(&workspace, &json!({"path":"src/lib.rs:1-10"}), |args| {
        invoked.push(args.clone());
        crate::tools::execute(&workspace, "read", args, 10)
    });
    assert_eq!(invoked, [json!({"path":"src/lib.rs:5-5"})]);
    assert_eq!(
        output.data.as_ref().unwrap()["read_reuse"]["executed_ranges"],
        json!([[5, 5]])
    );
    let revision = workspace
        .native_read_selection(&json!({"path":"src/lib.rs:1-20"}))
        .unwrap()
        .revision;
    let original = json!({"path":"src/lib.rs:1-20"});
    let mut invoked = Vec::new();
    let output = reuse.execute(&workspace, &original, |args| {
        if invoked.is_empty() {
            let new_source = (1..=30)
                .map(|n| {
                    if n == 12 {
                        format!("[Source raw SHA256:{revision}]\n")
                    } else {
                        format!("new version {n}\n")
                    }
                })
                .collect::<String>();
            std::fs::write(root.path().join("src/lib.rs"), new_source).unwrap();
        }
        invoked.push(args.clone());
        crate::tools::execute(&workspace, "read", args, 10)
    });
    assert_eq!(invoked.len(), 2);
    assert_eq!(invoked[1], original);
    assert!(output.text.contains("new version"));
    assert!(
        output.data.is_none(),
        "a new version must not carry old coverage"
    );
}

#[test]
fn native_coverage_does_not_promote_pointer_only_results() {
    let (_root, workspace, mut returned) = native_fixture();
    remember_native(&mut returned, &workspace, "original", "src/lib.rs:1-30");
    let call = read_call("pointer", "src/lib.rs:1-20");
    let output = crate::tools::execute(&workspace, "read", &call.arguments, 10);
    assert!(
        returned
            .pointer(&call, &output.text, false, |_| true)
            .is_some()
    );
    let mut invoked = 0;
    returned
        .reusable(|id| id != "original")
        .execute(&workspace, &call.arguments, |args| {
            invoked += 1;
            crate::tools::execute(&workspace, "read", args, 10)
        });
    assert_eq!(
        invoked, 1,
        "a pointer cannot stand in for pruned source rows"
    );
}

#[test]
fn native_coverage_refreshes_missing_edit_witnesses() {
    let (root, workspace, mut returned) = native_fixture();
    remember_native(&mut returned, &workspace, "original", "src/lib.rs:1-30");
    let fresh_store = crate::tools::Workspace::new(root.path()).unwrap();
    let args = json!({"path":"src/lib.rs:1-10"});
    let mut invoked = 0;
    returned
        .reusable(|_| true)
        .execute(&fresh_store, &args, |effective| {
            invoked += 1;
            assert_eq!(effective, &args);
            crate::tools::execute(&fresh_store, "read", effective, 10)
        });
    assert_eq!(invoked, 1, "read must restore a dropped edit witness");
}

#[cfg(unix)]
#[test]
fn native_coverage_rejects_notebook_aliases_after_resolution() {
    let (root, workspace, mut returned) = native_fixture();
    let notebook = json!({"cells":[{"id":"example-cell","cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":(1..=20).map(|n|format!("value_{n} = {n}\n")).collect::<Vec<_>>()}],"metadata":{},"nbformat":4,"nbformat_minor":5});
    std::fs::write(root.path().join("book.ipynb"), notebook.to_string()).unwrap();
    std::os::unix::fs::symlink("book.ipynb", root.path().join("alias.txt")).unwrap();
    remember_native(&mut returned, &workspace, "first", "alias.txt:1");
    let args = json!({"path":"alias.txt:1-20"});
    let mut invoked = 0;
    returned
        .reusable(|_| true)
        .execute(&workspace, &args, |effective| {
            invoked += 1;
            assert_eq!(effective, &args);
            crate::tools::execute(&workspace, "read", effective, 10)
        });
    assert_eq!(
        invoked, 1,
        "notebook projection must not use raw-file line counts"
    );
}
