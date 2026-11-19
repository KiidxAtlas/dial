use super::*;
#[test]
fn controller_calls_with_structured_arguments_get_readable_cards() {
    let call = |name: &str, args: Value| {
        let view = render_call(name, &args);
        (view.label, view.target)
    };
    use serde_json::json;
    assert_eq!(
        call(
            "verify",
            json!({"targets":["calc regression","tests/a.rs"]})
        ),
        ("Verify".into(), "calc regression, tests/a.rs".into())
    );
    assert_eq!(
        call(
            "verify",
            json!({"repro":{"name":"calc","command":"pytest -q"}})
        ),
        ("Verify".into(), "repro: calc".into())
    );
    assert_eq!(
        call(
            "verify",
            json!({"acceptance":{"name":"page","command":"curl x"}})
        ),
        ("Verify".into(), "acceptance: page".into())
    );
    assert_eq!(call("verify", json!({"plan":true})).1, "plan");
    assert_eq!(call("verify", json!({})).1, "selected checks");
    assert_eq!(
        call("verify", json!({"targets":["failed"]})).1,
        "rerun failed checks"
    );
    assert_eq!(
        call("finish", json!({"outcome":"completed","summary":"done"})),
        ("Finish".into(), "completed".into())
    );
}
#[test]
fn coherent_runtime_batched_reads_and_patch_targets_use_the_tool_contract() {
    assert_eq!(
        render_call(
            "read",
            &serde_json::json!({"paths":["src/a.rs:1-80","src/b.rs"]})
        )
        .target,
        "src/a.rs:1-80, src/b.rs"
    );
    let input = "*** Begin Patch\n*** Add File: src/new.rs\n+fn main() {}\n*** Delete File: src/old.rs\n*** End Patch";
    assert_eq!(
        render_call("edit", &serde_json::json!({"input":input})).target,
        "src/new.rs, src/old.rs"
    );
    assert_eq!(
        render_call(
            "edit",
            &serde_json::json!({"path":"src/lib.rs","old_string":"old","new_string":"new"})
        )
        .target,
        "src/lib.rs"
    );
    assert_eq!(
        render_call("edit", &serde_json::json!({"input":"malformed"})).target,
        "patch"
    );
}
#[test]
fn coherent_runtime_check_presentation_does_not_guess_from_command_text() {
    let ordinary = render_call("bash", &serde_json::json!({"command":"Verify parser"}));
    assert_eq!(ordinary.label, "Run");
    assert_eq!(ordinary.body, BodyStyle::Text);
    let check = render_call(
        "bash",
        &serde_json::json!({"command":"cargo test","_dial_check":"parser"}),
    );
    assert_eq!(
        (check.label.as_str(), check.target.as_str()),
        ("Check", "parser")
    );
    assert_eq!(check.body, BodyStyle::Quiet);
}
#[test]
fn eval_cells_are_labelled_eval_not_run() {
    let cell = render_call("eval", &serde_json::json!({"language":"js","code":"1 + 1"}));
    assert_eq!(cell.label, "Eval");
    assert_eq!(cell.tone, Tone::Tools);
}
#[test]
fn edit_results_with_new_language_server_errors_show_the_count_as_a_fact() {
    let output = ToolOutput::ok(
        "Edited src/a.rs\n[LSP advisory, not verification] 2 new error(s) after this edit:\nsrc/a.rs:3:1: expected `;`\nsrc/a.rs:9:5: unknown name\n".into(),
    );
    let view = render_result("edit", &serde_json::json!({"path":"src/a.rs"}), &output);
    assert!(
        view.facts.contains(&"2 new errors".to_owned()),
        "{:?}",
        view.facts
    );
    let clean = render_result(
        "edit",
        &serde_json::json!({"path":"src/a.rs"}),
        &ToolOutput::ok("Edited src/a.rs\n".into()),
    );
    assert!(clean.facts.iter().all(|fact| !fact.contains("error")));
}

#[test]
fn search_results_count_their_matches() {
    let args = serde_json::json!({"pattern":"println"});
    let view = render_result(
        "grep",
        &args,
        &ToolOutput::ok(
            "src/main.rs:3:    println!(\"hi\");\nsrc/main.rs-4-}\nsrc/lib.rs:10:println!()\n"
                .into(),
        ),
    );
    assert!(
        view.facts.contains(&"2 matches".to_owned()),
        "{:?}",
        view.facts
    );
    let none = render_result("grep", &args, &ToolOutput::ok("no matches\n".into()));
    assert!(
        none.facts.contains(&"no matches".to_owned()),
        "{:?}",
        none.facts
    );
}

#[test]
fn skill_reference_reads_are_distinct_from_loading_the_core_skill() {
    for (file, target) in [
        (None, "rust-best-practices"),
        (
            Some("references/handbook-11.md"),
            "rust-best-practices · references/handbook-11.md",
        ),
        (
            Some("references/handbook-04.md"),
            "rust-best-practices · references/handbook-04.md",
        ),
        (Some(""), "rust-best-practices"),
    ] {
        let mut args = serde_json::json!({"name": "rust-best-practices"});
        if let Some(file) = file {
            args["file"] = file.into();
        }
        let view = render_call("skill", &args);
        assert_eq!(view.label, "Skill");
        assert_eq!(view.target, target);
        assert_eq!(view.body, BodyStyle::Quiet);
    }
}

#[test]
fn desktop_calls_name_the_operation_without_dumping_input_or_code() {
    let view = render_call(
        "computer",
        &serde_json::json!({
            "action": "call", "chain": [
                {"method":"window", "args":["42"]},
                {"method":"type", "args":["private draft"]}
            ]
        }),
    );
    assert_eq!(view.label, "Desktop");
    assert_eq!(view.target, "window 42 · type");
    assert!(!view.target.contains("private draft"));
    let script = render_call(
        "computer",
        &serde_json::json!({"action":"run", "code":"private code"}),
    );
    assert_eq!(script.target, "script");
}

#[test]
fn desktop_results_summarize_windows_and_captures_without_replacing_evidence() {
    let data = serde_json::json!({
        "value":[{"id":"42", "app":"Editor", "title":"Project"}],
        "screenshots":[{"width":800,"height":600,"target":"42"}]
    });
    let output = ToolOutput::structured(data.clone());
    let view = render_result(
        "computer",
        &serde_json::json!({"action":"call", "chain":[{"method":"windows"}]}),
        &output,
    );
    assert!(view.facts.contains(&"1 window".to_owned()));
    assert!(view.facts.contains(&"1 screenshot".to_owned()));
    assert_eq!(view.preview.as_deref(), Some("Editor · Project"));
    assert_eq!(output.data, Some(data));
    let failed = render_result(
        "computer",
        &serde_json::json!({"action":"run"}),
        &ToolOutput::err("capture refused"),
    );
    assert!(
        failed.preview.is_none(),
        "errors must retain their actual evidence"
    );
}
