use super::*;
#[test]
fn coherent_runtime_plain_reports_preserve_declared_terminal_outcomes() {
    for (text, phase) in [
        ("Blocked: credentials are missing.", TurnOutcome::Blocked),
        (
            "Incomplete: verification still needs a live host.",
            TurnOutcome::Incomplete,
        ),
        (
            "Failed: the requested check returned errors.",
            TurnOutcome::Failed,
        ),
        (
            "Needs input: choose a destination.",
            TurnOutcome::NeedsInput,
        ),
    ] {
        assert_eq!(declared_outcome(text), Some(phase));
    }
    assert_eq!(
        declared_outcome("The parser rejects the string 'Blocked:'."),
        None
    );
    assert_eq!(declared_outcome("`Failed: example`"), None);
    assert_eq!(declared_outcome("Finished the requested fix."), None);
}

#[test]
fn consistent_ux_completed_finish_rejects_a_promise_but_accepts_a_report() {
    assert!(
        Finish::parse(&json!({"outcome":"completed","summary":"I will run the tests next."}))
            .is_err()
    );
    assert!(Finish::parse(&json!({"outcome":"completed","summary":"The tests passed and the parser fix is complete."})).is_ok());
}

fn todo() -> TodoList {
    let mut todo = TodoList::default();
    assert!(
        !todo
            .call(&json!({"op":"set","tasks":[{"content":"write code"}]}))
            .is_error
    );
    todo
}

#[test]
fn optional_progress_does_not_force_bookkeeping_rounds() {
    assert!(matches!(
        CompletionGuard.stopped(&todo()),
        StopAction::Accept
    ));
}

#[test]
fn stopping_with_only_blocked_tasks_reports_the_blockers() {
    let mut todo = todo();
    assert!(
        !todo
            .call(&json!({"op":"block","reason":"needs the staging password"}))
            .is_error
    );
    match CompletionGuard.stopped(&todo) {
        StopAction::Blocked(text) => assert!(
            text.contains("1. write code (blocked: needs the staging password)"),
            "{text}"
        ),
        _ => panic!("only blocked work remains"),
    }
    assert!(
        open_work(&todo).contains("write code"),
        "finish completed must still refuse blocked work"
    );
}

#[test]
fn explicit_clarification_is_not_a_rhetorical_question() {
    assert!(awaiting_input(
        "I found two export targets.\n\nCould you clarify which target to use?"
    ));
    assert!(awaiting_input("Can you provide the missing input file?"));
    assert!(!awaiting_input(
        "Why did this loop? Because the check never changed."
    ));
    assert!(!awaiting_input("The regression now passes. What changed?"));
    assert!(!awaiting_input(
        "Could you clarify the target?\n\nI'll proceed with the default."
    ));
    assert!(!awaiting_input("```\ncan you clarify?\n```"));
}

#[test]
fn needs_input_parses_optional_bounded_choices() {
    let finish = Finish::parse(&json!({
        "outcome":"needs_input",
        "summary":"Choose a destination",
        "reason":"Where should the file go?",
        "options":["Project root", "Temporary directory"]
    }))
    .unwrap();
    assert_eq!(finish.summary, "Choose a destination");
    assert_eq!(
        finish.question.as_deref(),
        Some("Where should the file go?")
    );
    assert_eq!(
        finish.options,
        vec!["Project root".to_owned(), "Temporary directory".to_owned()]
    );

    let typed_only = Finish::parse(&json!({
        "outcome":"needs_input",
        "summary":"Choose a destination",
        "reason":"Where should the file go?"
    }))
    .unwrap();
    assert!(typed_only.options.is_empty());

    let max_options = (0..8).map(|n| format!("Choice {n}")).collect::<Vec<_>>();
    let max_choices = Finish::parse(&json!({
        "outcome":"needs_input",
        "summary":"Choose a destination",
        "reason":"Where should the file go?",
        "options":max_options
    }))
    .unwrap();
    assert_eq!(max_choices.options.len(), 8);

    let too_many_options = (0..9).map(|n| format!("Choice {n}")).collect::<Vec<_>>();
    assert!(
        Finish::parse(&json!({
            "outcome":"needs_input",
            "summary":"Choose a destination",
            "reason":"Where should the file go?",
            "options":too_many_options
        }))
        .is_err()
    );

    assert!(
        Finish::parse(&json!({
            "outcome":"needs_input",
            "summary":"Choose a destination",
            "reason":"Where should the file go?",
            "options":[]
        }))
        .is_err()
    );
    assert!(
        Finish::parse(&json!({
            "outcome":"blocked",
            "summary":"Need a decision",
            "reason":"Which option?",
            "options":["Project root"]
        }))
        .is_err()
    );
}

#[test]
fn finish_schema_uses_a_provider_compatible_object_root() {
    let tool = schema();
    let parameters = &tool["function"]["parameters"];
    assert_eq!(parameters["type"], "object");
    for keyword in ["oneOf", "allOf", "anyOf"] {
        assert!(parameters.get(keyword).is_none(), "unsupported {keyword}");
    }
    assert_eq!(parameters["required"], json!(["outcome", "summary"]));
}

#[test]
fn explicit_outcomes_require_concrete_nonempty_details() {
    for outcome in ["blocked", "needs_input"] {
        assert!(Finish::parse(&json!({"outcome":outcome,"summary":"Need help"})).is_err());
        assert!(
            Finish::parse(&json!({"outcome":outcome,"summary":"Need help","reason":" "})).is_err()
        );
        assert!(
            Finish::parse(
                &json!({"outcome":outcome,"summary":"Need help","reason":"Which output directory?"})
            )
            .is_ok()
        );
    }
    assert!(Finish::parse(&json!({"outcome":"completed","summary":" "})).is_err());
    assert!(Finish::parse(&json!({"outcome":"completed","summary":"Done"})).is_ok());
    assert!(
        Finish::parse(&json!({
            "outcome":"completed","summary":"Done","options":["Choice"]
        }))
        .is_err()
    );
    assert!(Finish::parse(&json!({"outcome":"unknown","summary":"Done"})).is_err());
}
