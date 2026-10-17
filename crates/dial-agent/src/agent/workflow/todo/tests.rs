use super::*;

#[test]
fn coherent_runtime_investigations_use_the_same_simple_completion() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content":"Inspect","investigation":true},{"content":"Edit"}]),
    );
    assert_eq!(list.completion_hint(), Some(json!({"op":"done"})));
    assert!(!list.call(&json!({"op":"done"})).is_error);
    assert_eq!(list.tasks()[0].status, Status::Completed);
    assert!(list.tasks()[0].finding.is_none());
    assert_eq!(list.tasks()[1].status, Status::InProgress);
    let mut resumed = TodoList::new();
    resumed.restore(&list.snapshot()).unwrap();
    assert_eq!(resumed, list);
    assert!(
        !list
            .call(&json!({"op":"block","reason":"Missing fixture"}))
            .is_error
    );
    list.complete_reported_work();
    assert_eq!(list.tasks()[1].status, Status::Blocked);
}

#[test]
fn accepted_report_closes_only_the_started_task_and_leaves_later_work_open() {
    let mut list = TodoList::new();
    assert!(
        !set(
            &mut list,
            json!([{"content":"One"},{"content":"Two"},{"content":"Three"}])
        )
        .is_error
    );
    list.complete_reported_work();
    assert_eq!(
        statuses(&list),
        [Status::Completed, Status::Pending, Status::Pending]
    );
    let mut resumed = TodoList::new();
    resumed.restore(&list.snapshot()).unwrap();
    assert_eq!(resumed, list);
    assert!(resumed.has_open());
}

#[test]
fn detailed_findings_close_tasks_and_survive_resume_without_truncation() {
    for answer in ["x".repeat(633), "界".repeat(4_000)] {
        let mut list = TodoList::new();
        assert!(
            !set(
                &mut list,
                json!([
                    {"content":"Trace the failure", "investigation":true},
                    {"content":"Implement the correction"}
                ])
            )
            .is_error
        );
        let output =
            list.call(&json!({"op":"done","finding":answer,"evidence":["src/parser.rs:24"]}));
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(list.tasks()[0].finding.as_deref(), Some(answer.as_str()));
        assert_eq!(list.tasks()[1].status, Status::InProgress);
        let mut resumed = TodoList::new();
        resumed.restore(&list.snapshot()).unwrap();
        assert_eq!(resumed, list);
        assert!(resumed.focus().unwrap().contains(&answer));
        assert!(!resumed.call(&json!({"op":"start","index":1})).is_error);
        let before = resumed.snapshot();
        assert!(resumed.call(&json!({"op":"done","finding":"x".repeat(4_001),"evidence":["src/parser.rs:24"]})).is_error);
        assert_eq!(resumed.snapshot(), before);
    }
}

#[test]
fn optional_investigation_findings_are_validated_and_preserved() {
    let mut list = TodoList::new();
    assert!(
        !set(
            &mut list,
            json!([
                {"content":"Where is the limit enforced?", "investigation":true},
                {"content":"Change the limit and test"}
            ])
        )
        .is_error
    );
    let before = list.snapshot();
    let args = json!({"op":"done","finding":"In the parser","evidence":[]});
    assert!(list.call(&args).is_error);
    assert_eq!(list.snapshot(), before);
    assert!(!list.call(&json!({"op":"done", "finding":"The parser rejects values above 5", "evidence":["src/parser.rs:24"]})).is_error);
    assert!(list.render().contains("Finding: The parser"));
    let mut resumed = TodoList::new();
    resumed.restore(&list.snapshot()).unwrap();
    assert_eq!(resumed, list);
    assert!(resumed.focus().unwrap().contains("src/parser.rs:24"));
    resumed.call(&json!({"op":"start","index":1}));
    assert!(resumed.tasks()[0].finding.is_none());
    assert!(resumed.tasks()[0].evidence.is_empty());
}

#[test]
fn legacy_completed_investigations_restore_without_a_proof_payload() {
    let mut list = TodoList::new();
    assert!(
        !set(
            &mut list,
            json!([{"content":"Why?","investigation":true,"status":"completed"}])
        )
        .is_error
    );
    let mut restored = TodoList::new();
    restored.restore(&list.snapshot()).unwrap();
    assert_eq!(list, restored);
    assert!(!restored.has_open());
}

#[test]
fn rejected_plan_returns_preserved_state_and_actionable_recovery() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content":"Inspect clear", "investigation":true}]),
    );
    let before = list.snapshot();
    let output = set(
        &mut list,
        json!([
            {"content":"Inspect clear", "investigation":true, "status":"unknown"}
        ]),
    );
    assert!(output.is_error);
    assert!(output.text.contains("status"));
    assert!(output.text.contains("Current plan:"));
    assert!(output.text.contains("Inspect clear"));
    assert_eq!(output.data.unwrap()["tasks"], before);
    assert_eq!(list.snapshot(), before);
    assert!(
        !list
            .call(&json!({
                "op":"done", "finding":"Clear only repaints", "evidence":["src/app.rs:2994"]
            }))
            .is_error
    );
}

fn set(list: &mut TodoList, tasks: Value) -> ToolOutput {
    list.call(&json!({"op": "set", "tasks": tasks}))
}

fn statuses(list: &TodoList) -> Vec<Status> {
    list.tasks().iter().map(|t| t.status).collect()
}

#[test]
fn focus_names_the_current_and_next_step_only_for_multi_step_plans() {
    let mut list = TodoList::new();
    set(&mut list, json!([{"content": "only step"}]));
    assert_eq!(list.focus(), None);
    set(
        &mut list,
        json!([{"content": "read", "status": "completed"}, {"content": "fix"}, {"content": "test"}]),
    );
    let focus = list.focus().unwrap();
    assert!(
        focus.contains("(1/3 done): 2. fix.") && focus.contains("Next: 3. test."),
        "{focus}"
    );
    list.call(&json!({"op": "done"}));
    assert!(list.focus().unwrap().contains("last open step"));
    list.call(&json!({"op": "block", "reason": "flaky CI"}));
    assert_eq!(
        list.focus(),
        None,
        "nothing in progress means nothing to focus on"
    );
}

#[test]
fn set_keeps_explicit_statuses_and_renders_checkboxes() {
    let mut list = TodoList::new();
    let out = set(
        &mut list,
        json!([
            {"content": "read code", "status": "completed"},
            {"content": "write fix", "status": "in_progress"},
            {"content": "run tests"},
            {"content": "old idea", "status": "abandoned"}
        ]),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        list.render(),
        "[x] 1. read code\n[~] 2. write fix\n[ ] 3. run tests\n[-] 4. old idea"
    );
    assert!(out.text.contains("2/4 done"), "{}", out.text);
}

#[test]
fn closing_the_active_task_starts_the_next_pending_one() {
    let mut list = TodoList::new();
    let out = set(
        &mut list,
        json!([{"content": "a"}, {"content": "b"}, {"content": "c"}]),
    );
    assert!(
        out.text.contains("Started 1. a"),
        "a plan with nothing active starts its first task: {}",
        out.text
    );
    assert_eq!(
        statuses(&list),
        [Status::InProgress, Status::Pending, Status::Pending]
    );

    let out = list.call(&json!({"op": "done"}));
    assert!(
        !out.is_error && out.text.starts_with("Completed 1. a. Started 2. b."),
        "{}",
        out.text
    );
    assert_eq!(
        statuses(&list),
        [Status::Completed, Status::InProgress, Status::Pending]
    );

    // Closing a task other than the active one leaves the active one alone.
    list.call(&json!({"op": "drop", "index": 3, "reason": "not needed"}));
    assert_eq!(
        statuses(&list),
        [Status::Completed, Status::InProgress, Status::Abandoned]
    );
    assert!(list.render().contains("3. c (dropped: not needed)"));

    list.call(&json!({"op": "done"}));
    assert!(!list.has_open());
    assert_eq!(list.progress(), (3, 3));
}

#[test]
fn closed_tasks_are_pruned_to_a_recent_tail() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([
            {"content": "t1", "status": "completed"},
            {"content": "t2", "status": "completed"},
            {"content": "t3", "status": "completed"},
            {"content": "t4", "status": "completed"},
            {"content": "t5"}
        ]),
    );
    assert_eq!(
        list.render(),
        "[x] 1. t2\n[x] 2. t3\n[x] 3. t4\n[~] 4. t5",
        "the oldest closed task is pruned and numbering closes up"
    );
    assert_eq!(list.progress(), (3, 4));

    // Closing the last task keeps it listed as the newest closed one.
    list.call(&json!({"op": "done"}));
    assert_eq!(list.render(), "[x] 1. t3\n[x] 2. t4\n[x] 3. t5");
    assert!(!list.has_open());
}

#[test]
fn pruning_keeps_the_findings_tail_and_resumes_clean() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([
            {"content": "q1", "investigation": true},
            {"content": "q2", "investigation": true},
            {"content": "q3", "investigation": true},
            {"content": "q4", "investigation": true},
            {"content": "step"}
        ]),
    );
    for (q, finding) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3"), ("q4", "a4")] {
        let out = list.call(&json!({
            "op": "done",
            "finding": finding,
            "evidence": [format!("{q}:1")]
        }));
        assert!(!out.is_error, "{}", out.text);
    }
    assert_eq!(
        list.tasks()
            .iter()
            .map(|t| t.content.as_str())
            .collect::<Vec<_>>(),
        ["q2", "q3", "q4", "step"],
        "the oldest closed question is pruned once four are closed"
    );
    let focus = list.focus().unwrap();
    assert!(
        focus.contains("a2") && focus.contains("a3") && focus.contains("a4"),
        "{focus}"
    );
    assert!(!focus.contains("a1"), "{focus}");

    // A resumed snapshot loads pruned the same way.
    let mut resumed = TodoList::new();
    resumed
        .restore(&json!([
            {"content": "c1", "status": "completed"},
            {"content": "c2", "status": "completed"},
            {"content": "c3", "status": "completed"},
            {"content": "c4", "status": "abandoned", "note": "superseded"},
            {"content": "open", "status": "pending"}
        ]))
        .unwrap();
    assert_eq!(
        resumed
            .tasks()
            .iter()
            .map(|t| t.content.as_str())
            .collect::<Vec<_>>(),
        ["c2", "c3", "c4", "open"]
    );
}

#[test]
fn block_needs_a_reason_keeps_work_open_and_start_unblocks() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content": "deploy"}, {"content": "write docs"}]),
    );
    let out = list.call(&json!({"op": "block"}));
    assert!(out.is_error && out.text.contains("reason"), "{}", out.text);
    assert_eq!(
        statuses(&list),
        [Status::InProgress, Status::Pending],
        "a rejected op must not change the list"
    );

    let out = list.call(&json!({"op": "block", "reason": "no deploy credentials"}));
    assert!(
        out.text.contains("Started 2. write docs") && out.text.contains("1 blocked"),
        "{}",
        out.text
    );
    assert_eq!(statuses(&list), [Status::Blocked, Status::InProgress]);
    list.call(&json!({"op": "done"}));
    assert!(list.has_open(), "a blocked task is still owed");
    assert_eq!(
        list.tasks()[0].label(1),
        "1. deploy (blocked: no deploy credentials)"
    );

    list.call(&json!({"op": "start", "index": 1}));
    assert_eq!(list.tasks()[0].status, Status::InProgress);
    assert_eq!(list.tasks()[0].note, None);
}

#[test]
fn detailed_drop_and_block_reasons_survive_resume_and_keep_state_on_overflow() {
    for op in ["drop", "block"] {
        for why in ["x".repeat(350), "𐍈".repeat(MAX_REASON_CHARS)] {
            let mut list = TodoList::new();
            set(
                &mut list,
                json!([{"content":"inspect baseline"},{"content":"next step"}]),
            );
            let output = list.call(&json!({"op":op,"reason":why}));
            assert!(!output.is_error, "{}", output.text);
            assert_eq!(list.tasks()[0].note.as_deref(), Some(why.as_str()));
            assert_eq!(
                list.tasks()[0].status,
                if op == "drop" {
                    Status::Abandoned
                } else {
                    Status::Blocked
                }
            );
            assert_eq!(list.tasks()[1].status, Status::InProgress);
            assert!(list.render().contains(&why));
            let mut resumed = TodoList::new();
            resumed.restore(&list.snapshot()).unwrap();
            assert_eq!(resumed, list);

            let before = resumed.snapshot();
            let oversized = "𐍈".repeat(MAX_REASON_CHARS + 1);
            let failed = resumed.call(&json!({"op":op,"index":2,"reason":oversized}));
            assert!(
                failed.is_error && failed.text.contains("limit is 4000"),
                "{}",
                failed.text
            );
            assert_eq!(resumed.snapshot(), before);
            let mut bad_snapshot = before.clone();
            bad_snapshot[0]["note"] = json!(oversized);
            assert!(resumed.restore(&bad_snapshot).is_err());
            assert_eq!(resumed.snapshot(), before);
        }
    }
    assert_eq!(
        schema()["function"]["parameters"]["properties"]["reason"]["maxLength"],
        MAX_REASON_CHARS
    );
}

#[test]
fn start_switches_focus_and_reopens() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content": "a"}, {"content": "b", "status": "completed"}]),
    );
    let out = list.call(&json!({"op": "start", "index": 2}));
    assert!(
        out.text.starts_with("Paused 1. Started 2. b."),
        "{}",
        out.text
    );
    assert_eq!(statuses(&list), [Status::Pending, Status::InProgress]);
    assert!(!list.call(&json!({"op": "start"})).is_error);
    assert_eq!(
        statuses(&list),
        [Status::InProgress, Status::Pending],
        "start without an index resumes the first pending task"
    );
}

#[test]
fn phases_group_the_rendering_and_are_inherited() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([
            {"content": "read", "phase": "Investigate"},
            {"content": "trace"},
            {"content": "fix", "phase": "Implement"}
        ]),
    );
    list.call(&json!({"op": "append", "tasks": [{"content": "test"}]}));
    assert_eq!(
        list.render(),
        "Investigate:\n  [~] 1. read\n  [ ] 2. trace\nImplement:\n  [ ] 3. fix\n  [ ] 4. test"
    );
}

#[test]
fn caps_and_bad_input_are_errors_that_leave_state_alone() {
    let mut list = TodoList::new();
    let many: Vec<Value> = (0..=MAX_TASKS)
        .map(|i| json!({"content": format!("t{i}")}))
        .collect();
    assert!(set(&mut list, Value::Array(many)).is_error);
    let exactly: Vec<Value> = (0..MAX_TASKS)
        .map(|i| json!({"content": format!("t{i}")}))
        .collect();
    assert!(!set(&mut list, Value::Array(exactly)).is_error);
    assert!(
        list.call(&json!({"op": "append", "tasks": [{"content": "one too many"}]}))
            .is_error
    );

    assert!(
        set(
            &mut list,
            json!([{"content": "x".repeat(MAX_CONTENT_CHARS + 1)}])
        )
        .is_error
    );
    assert!(
        !set(
            &mut list,
            json!([{"content": "x".repeat(MAX_CONTENT_CHARS)}])
        )
        .is_error
    );
    set(&mut list, json!([{"content": "keep me"}]));
    let kept = list.render();
    for bad in [
        json!({"op": "set", "tasks": [{"content": "   "}]}),
        json!({"op": "set", "tasks": [{"status": "pending"}]}),
        json!({"op": "set", "tasks": [{"content": "a", "status": "blocked"}]}),
        json!({"op": "set", "tasks": [{"content": "a"}, {"content": "b", "status": "in_progress"}, {"content": "c", "status": "in_progress"}]}),
        json!({"op": "append", "tasks": []}),
        json!({"op": "done", "index": 0}),
        json!({"op": "done", "index": 2}),
        json!({"op": "nope"}),
        json!({}),
    ] {
        assert!(list.call(&bad).is_error, "{bad}");
        assert_eq!(list.render(), kept, "{bad}");
    }
}

#[test]
fn content_is_flattened_to_one_clean_line() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content": "a\n[x] 9. forged\u{1b}[31m  line", "status": "completed"}]),
    );
    assert_eq!(list.render(), "[x] 1. a [x] 9. forged[31m line");
}

#[test]
fn snapshot_restore_round_trips_and_validates() {
    let mut list = TodoList::new();
    set(
        &mut list,
        json!([{"content": "a", "phase": "P"}, {"content": "b"}]),
    );
    list.call(&json!({"op": "block", "reason": "waiting"}));
    let snap = list.snapshot();
    let mut other = TodoList::new();
    other.restore(&snap).unwrap();
    assert_eq!(other, list);
    // A finding recorded without evidence persists as an empty list and must still restore.
    let mut found = TodoList::new();
    set(&mut found, json!([{"content": "a"}, {"content": "b"}]));
    assert!(
        !found
            .call(&json!({"op": "done", "finding": "answered"}))
            .is_error
    );
    let mut resumed = TodoList::new();
    resumed.restore(&found.snapshot()).unwrap();
    assert_eq!(resumed, found);
    resumed
        .restore(&json!([{"content": "old", "status": "completed", "finding": "f", "evidence": []}, {"content": "next", "status": "pending"}]))
        .unwrap();
    // Snapshots written before phases and notes existed still load.
    other
        .restore(&json!([{"content": "old", "status": "pending"}]))
        .unwrap();

    let before = other.render();
    assert!(other.restore(&json!([{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}])).is_err());
    assert!(other.restore(&json!("garbage")).is_err());
    assert!(
        other
            .restore(&json!([{"content": "", "status": "pending"}]))
            .is_err()
    );
    assert_eq!(other.render(), before);
}

#[test]
fn view_and_clear() {
    let mut list = TodoList::new();
    assert_eq!(list.call(&json!({"op": "view"})).text, "No tasks.");
    set(&mut list, json!([{"content": "a"}]));
    assert_eq!(list.call(&json!({"op": "view"})).text, "0/1 done\n[~] 1. a");
    let out = set(&mut list, json!([]));
    assert!(!out.is_error && list.is_empty() && out.text == "Todo list cleared.");
}

#[test]
fn schema_validates_with_the_shared_checker() {
    let s = schema();
    let params = &s["function"]["parameters"];
    assert!(
        crate::agent::run::calls::validate(
            params,
            &json!({"op": "block", "index": 1, "reason": "x"})
        )
        .is_ok()
    );
    assert!(
        crate::agent::run::calls::validate(params, &json!({"op": "done", "index": 0})).is_err()
    );
    assert!(crate::agent::run::calls::validate(params, &json!({"op": "update"})).is_err());
    assert!(crate::agent::run::calls::validate(params, &json!({"tasks": []})).is_ok());
}

#[test]
fn lenient_shapes_reach_the_tool_and_infer_the_obvious_target() {
    let mut list = TodoList::new();
    assert!(!list.call(&json!({"tasks": [{"content": "Edit"}]})).is_error);
    assert!(!list.call(&json!({"tasks": [{"content": "Test"}]})).is_error);
    assert_eq!(list.tasks.len(), 2, "missing op appends once a plan exists");
    assert!(!list.call(&json!({"op": "done"})).is_error);
    assert_eq!(list.tasks[0].status, Status::Completed);
    assert_eq!(list.tasks[1].status, Status::InProgress);
    assert!(
        !list
            .call(&json!({"tasks": [{"content": "Third"}]}))
            .is_error
    );
    assert!(!list.call(&json!({"op": "start"})).is_error);
    assert_eq!(
        list.tasks[2].status,
        Status::InProgress,
        "start picks the first pending task"
    );
    assert_eq!(list.tasks[1].status, Status::Pending);
}

#[test]
fn findings_alias_and_unknown_keys_get_a_corrected_call() {
    let mut list = TodoList::new();
    assert!(
        !list
            .call(&json!({"tasks": [{"content": "Inspect", "investigation": true}]}))
            .is_error
    );
    let out = list.call(&json!({"op": "done", "findings": "ans", "evidence": ["a.rs:1"]}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(list.tasks[0].finding.as_deref(), Some("ans"));
    let out = list.call(&json!({"op": "done", "done": true}));
    assert!(out.is_error);
    assert!(out.text.contains("unknown key \"done\""), "{}", out.text);
    assert!(out.text.contains("{\"op\":\"done\"}"), "{}", out.text);
}

#[test]
fn unknown_item_keys_name_the_allowed_keys() {
    let mut list = TodoList::new();
    let out = list.call(&json!({"tasks": [{"content": "A", "done": true}]}));
    assert!(out.is_error);
    assert!(out.text.contains("unknown key \"done\""), "{}", out.text);
    assert!(out.text.contains("content, status, phase, investigation"));
}

#[test]
fn missing_op_without_tasks_gets_the_usage_hint() {
    let mut list = TodoList::new();
    let out = list.call(&json!({"index": 1}));
    assert!(out.is_error);
    assert!(out.text.contains("{\"op\":\"done\"}"), "{}", out.text);
}
