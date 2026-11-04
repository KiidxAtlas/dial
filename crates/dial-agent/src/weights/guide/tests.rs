use super::*;

#[test]
fn presets_expand_only_exact_names() {
    assert!(expand_goal(" loops ").contains("repetition"));
    assert_eq!(expand_goal("loops in lists"), "loops in lists");
}

#[test]
fn cases_keep_text_turns_and_end_in_the_user_turn() {
    let messages = vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "first"}),
        json!({"role": "assistant", "content": null, "tool_calls": [{"id": "t"}]}),
        json!({"role": "tool", "content": "result"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
        json!({"role": "user", "content": "second"}),
        json!({"role": "user", "content": "   "}),
    ];
    let cases = cases_from_messages(&messages, 8, 4);
    assert_eq!(
        cases,
        vec![
            json!({"messages": [{"role": "user", "content": "first"}]}),
            json!({"messages": [{"role": "user", "content": "first"},
                                {"role": "assistant", "content": "answer"},
                                {"role": "user", "content": "second"}]}),
        ]
    );
    // Without context each case is the lone turn; the limit keeps the newest.
    assert_eq!(
        cases_from_messages(&messages, 1, 0),
        vec![json!({"messages": [{"role": "user", "content": "second"}]})]
    );
}

#[test]
fn next_steps_follow_the_edit_loop() {
    let base = json!({"target": {"path": "/m"}, "progress": null, "analyze": null,
                      "edits": [], "experiments": []});
    let steps = |status: &Value| next_steps(status).join("\n");
    assert!(steps(&json!({"target": {"unsupported": "x"}})).contains("--provider mlx"));
    let fresh = steps(&base);
    assert!(fresh.contains("weights analyze") && fresh.contains("weights experiment"));
    let mut status = base.clone();
    status["analyze"] = json!({});
    status["experiments"] = json!([{"experiment": "e", "budget": 4, "used": 1,
                                    "runs": [{"op": "generate"}]}]);
    assert!(steps(&status).contains("weights attribute --experiment e"));
    status["edits"] = json!([{"name": "x1", "passed": null}, {"name": "x2", "passed": true}]);
    let edits = steps(&status);
    assert!(
        edits.contains("evaluate x1") && edits.contains("promote x2"),
        "{edits}"
    );
}
