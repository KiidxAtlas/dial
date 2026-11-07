use super::*;
use crate::runtime::tool_output::ToolOutput;

#[test]
fn answered_is_a_delivery_outcome_not_a_completion_or_input_claim() {
    let finish =
        Finish::parse(&json!({"outcome":"answered","summary":"Here is the explanation."})).unwrap();
    assert_eq!(finish.outcome, TurnOutcome::Answered);
    assert_eq!(finish.text, "Here is the explanation.");
    assert!(finish.question.is_none());
    assert!(finish.options.is_empty());
    assert!(Finish::parse(&json!({"outcome":"answered","summary":" "})).is_err());
    let report = crate::agent::TurnReport {
        text: finish.text,
        outcome: finish.outcome,
        steps: 1,
        interrupted: false,
        trajectory: None,
    };
    assert!(report.halts_automation());
}

#[test]
fn outstanding_notice_is_bounded_and_does_not_promote_follow_ups_to_requirements() {
    let mut todo = todo();
    assert!(!todo.call(&json!({"op":"append","tasks":[{"content":"Optional live check","kind":"follow_up"}]})).is_error);
    assert!(
        !todo
            .call(&json!({"op":"block","index":2,"reason":"x".repeat(4000)}))
            .is_error
    );
    let notice = outstanding_notice(&todo, &json!({}), TurnOutcome::Answered).unwrap();
    assert!(
        notice.contains("1 execution step(s), 0 required deliverable(s), 1 optional follow-up(s)"),
        "{notice}"
    );
    assert!(notice.contains("does not mark that work complete"));
    assert!(notice.len() < 1000);
    assert!(todo.blocked_steps().next().is_none());
    assert!(outstanding_notice(&TodoList::new(), &json!({}), TurnOutcome::Answered).is_none());
}
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
fn finish_parsing_validates_shape_and_leaves_completion_to_the_controller() {
    assert!(
        Finish::parse(&json!({"outcome":"completed","summary":"I will run the tests next."}))
            .is_ok()
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
    assert!(todo().blocked_steps().next().is_none());
}

#[test]
fn only_executed_runtime_checks_count_and_a_caught_failure_remains_visible() {
    let prose = ToolOutput::ok("assert(true); checks: passed".into());
    assert!(eval_checks(&prose).next().is_none());
    let output = ToolOutput::structured(json!({"checks":[{"name":"tiny round trip","passed":true},
        {"name":"saved output","passed":false},{"name":"not an outcome"}]}));
    assert_eq!(eval_checks(&output).collect::<Vec<_>>(), [true, false]);
}

#[test]
fn stopping_with_only_blocked_tasks_reports_the_blockers() {
    let mut todo = todo();
    assert!(
        !todo
            .call(&json!({"op":"block","reason":"needs the staging password"}))
            .is_error
    );
    assert_eq!(todo.blocked_steps().count(), 1);
    let text = outstanding_notice(&todo, &json!({}), TurnOutcome::Answered).unwrap();
    assert!(
        text.contains("1. write code (blocked: needs the staging password)"),
        "{text}"
    );
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

#[test]
fn capability_denials_are_recognized_in_prose_only() {
    for denial in [
        "I cannot run the game here because this environment has no interactive terminal.",
        "Unfortunately a browser is not available to me.",
        "I don't have a tool for debugging Python.",
        "I’m unable to open a browser here.",
        "Running curses programs is not possible for me without a TTY!",
    ] {
        assert!(denies_capability(denial), "{denial}");
    }
    for answer in [
        "Fixed the parser; all 12 tests pass.",
        "The function cannot return null, so the guard was removed.",
        "```\nerror: terminal not available\n```\nThe flaky test now passes.",
        // Prose about the project, not about the agent's own abilities.
        "This env var is not available in the test environment.",
        "Running curses programs is not possible without a TTY!",
    ] {
        assert!(!denies_capability(answer), "{answer}");
    }
}

#[test]
fn capability_search_reminds_once_and_never_after_a_search() {
    let denial = "I cannot open an interactive terminal in this environment.";
    let mut search = CapabilitySearch::default();
    assert!(!search.remind(denial, false), "nothing to load");
    assert!(!search.remind("Done; tests pass.", true));
    assert!(search.remind(denial, true));
    assert!(!search.remind(denial, true), "one reminder per instruction");

    let mut searched = CapabilitySearch::default();
    searched.searched();
    assert!(!searched.remind(denial, true));
}
