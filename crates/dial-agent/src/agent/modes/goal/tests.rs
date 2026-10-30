use super::*;
use crate::agent::NullUi;
use crate::config::types::{Config, Version};
use crate::llm::fake::{self, Script};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::PathBuf;

fn sse_reply(text: &str) -> Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// A reply with a usage report, so the turn's tokens reach the goal's budget.
fn sse_reply_with_usage(text: &str, input: u64, output: u64) -> Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    let usage = json!({"choices":[],"usage":{"prompt_tokens":input,"completion_tokens":output}});
    Script::Sse(format!(
        "data: {event}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
    ))
}

fn sse_tool_call(name: &str, args: Value) -> Script {
    let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call_{name}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
    Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn new_agent<'a>(config: &'a Config, url: &str, workspace: &std::path::Path) -> Agent<'a> {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
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
    Agent::hermetic(config, backend, &version, workspace, None, None).unwrap()
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "alpha\n").unwrap();
    dir
}

#[test]
fn rendering_preserves_literal_template_markers_in_objective() {
    let mut state = State::default();
    state.start(
        "Check <tag> & {{objective}} {{tokensUsed}} {{tokenBudget}} {{remainingTokens}} {{timeUsedSeconds}}",
        Some(100),
    );
    state.account(12, 3);
    let goal = state.goal.as_ref().unwrap();
    for template in [GOAL_MODE_ACTIVE, CONTINUATION, BUDGET_LIMIT] {
        let prompt = render(template, goal);
        assert!(prompt.contains("<objective>\nCheck &lt;tag&gt; &amp; {{objective}} {{tokensUsed}} {{tokenBudget}} {{remainingTokens}} {{timeUsedSeconds}}\n</objective>"));
        assert!(prompt.contains("Tokens used: 12"));
        assert!(prompt.contains("Token budget: 100"));
        if template != BUDGET_LIMIT {
            assert!(prompt.contains("Tokens remaining: 88"));
        }
        assert!(prompt.contains("Time used: 3 seconds"));
    }
}

#[test]
fn launch_modes_start_the_goal_and_set_launch_state() {
    let config = Config::default();
    let dir = tempfile::tempdir().unwrap();
    let mut agent = new_agent(&config, "http://127.0.0.1:9", dir.path());
    // The launch sequence `cli::mode_flags` applies for `--goal` with advisor and external thinking.
    agent.set_launch_modes(true, true);
    agent
        .goal_mutate(|goal| goal.start("ship it", None))
        .unwrap();
    let objective = agent
        .goal_mutate(|state| state.goal.as_ref().map(|goal| goal.objective.clone()))
        .unwrap();
    assert_eq!(objective.as_deref(), Some("ship it"));
    assert!(agent.advisor_enabled());
    assert!(agent.external_thinking_instruction().is_some());
}

fn messages(request: &[u8]) -> String {
    fake::body(request)["messages"].to_string()
}

fn coached_config(url: &str) -> Config {
    let mut config = Config::default();
    config.model_roles.insert(
        "coach".into(),
        crate::llm::model_config::ModelSelection {
            provider: "endpoint".into(),
            model: "coach-test".into(),
            base_url: Some(format!("{url}/v1")),
            ..Default::default()
        },
    );
    config
}

#[test]
fn coach_repairs_in_active_loop_and_reviews_local_evidence() {
    let server = fake::serve(vec![
        sse_tool_call("read", json!({"path":"missing.txt"})),
        sse_reply("The file was checked."),
        sse_reply(
            r#"{"gaps":[{"requirement":"inspect existing notes","evidence":1,"next_check":"read notes.txt"}]}"#,
        ),
        sse_tool_call("read", json!({"path":"notes.txt"})),
        sse_reply("Inspected notes: alpha."),
        sse_reply(r#"{"gaps":[]}"#),
    ]);
    let config = coached_config(&server.url);
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.set_advisor(true);
    let report = agent.turn("Inspect existing notes", &mut NullUi).unwrap();
    assert_eq!(report.text, "Inspected notes: alpha.");
    assert_eq!(report.steps, 4, "coach requests do not start a new turn");
    let requests = server.requests();
    assert_eq!(requests.len(), 6);
    assert_eq!(fake::body(&requests[2])["model"], "coach-test");
    assert!(messages(&requests[2]).contains("missing.txt"));
    assert!(messages(&requests[3]).contains("read notes.txt"));
    assert!(messages(&requests[5]).contains("alpha"));
    assert!(
        !config.escalation.enabled,
        "advisor must not enable automatic escalation"
    );
}

#[test]
fn coach_finish_repairs_once_and_discloses_remaining_gaps() {
    let gap =
        r#"{"gaps":[{"requirement":"inspect notes","evidence":0,"next_check":"read notes.txt"}]}"#;
    let server = fake::serve(vec![
        sse_tool_call("finish", json!({"outcome":"completed","summary":"Done."})),
        sse_reply(gap),
        sse_reply("Inspected notes."),
        sse_reply(gap),
    ]);
    let mut config = coached_config(&server.url);
    // Keep the explicit finish-only offer stable in this hermetic fixture.
    config.image.auto = false;
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.set_advisor(true);
    agent
        .push(json!({"role":"user","content":"Inspect notes"}))
        .unwrap();
    let mut backend = agent.backend.take().unwrap();
    let mut schemas = vec![crate::agent::verify::completion::schema()];
    let (report, _) = agent
        .run_loop(
            &mut backend,
            &["finish".into()],
            &mut schemas,
            4,
            "Inspect notes",
            &mut NullUi,
        )
        .unwrap();
    assert!(
        report.text.contains("Unresolved coach findings"),
        "{}; tool results: {:?}",
        report.text,
        agent
            .session
            .messages()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect::<Vec<_>>()
    );
    assert_eq!(server.requests().len(), 4);
}

#[test]
fn accepted_finish_skips_live_advice_but_keeps_completion_review() {
    use crate::agent::verify::completion::TurnOutcome;
    #[derive(Default)]
    struct CoachEvents(Vec<String>);
    impl crate::agent::Ui for CoachEvents {
        fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
            if event.kind == crate::agent::ui::ui_event::EventKind::Coach {
                self.0.push(event.text.clone());
            }
        }
    }
    for (outcome, expected) in [
        ("completed", TurnOutcome::Completed),
        ("blocked", TurnOutcome::Blocked),
        ("needs_input", TurnOutcome::NeedsInput),
    ] {
        let mut args = json!({"outcome":outcome,"summary":"Inspected notes."});
        if outcome != "completed" {
            args["reason"] = json!("Which file should be inspected next?");
        }
        // Spare malformed coach replies let a redundant live review run and fail visibly instead
        // of waiting on a closed server. Unused scripts are never awaited by this test.
        let server = fake::serve(vec![
            sse_tool_call("finish", args),
            sse_reply("not JSON"),
            sse_reply("not JSON"),
        ]);
        let mut config = coached_config(&server.url);
        config.provider = Some("endpoint".into());
        config.model = "test".into();
        config.image.auto = false;
        let dir = workspace();
        let mut agent = new_agent(&config, &server.url, dir.path());
        // Exercise the live advisor, which is intentionally disabled in hermetic mode.
        agent.hermetic = false;
        agent.set_advisor(true);
        agent
            .push(json!({"role":"user","content":"Inspect notes"}))
            .unwrap();
        let mut backend = agent.backend.take().unwrap();
        let mut schemas = vec![crate::agent::verify::completion::schema()];
        let mut ui = CoachEvents::default();
        let (report, _) = agent
            .run_loop(
                &mut backend,
                &["finish".into()],
                &mut schemas,
                4,
                "Inspect notes",
                &mut ui,
            )
            .unwrap();
        assert_eq!(report.outcome, expected, "{outcome}");
        assert!(
            ui.0.iter().all(|event| !event.contains("round review")),
            "{outcome}: terminal finish started live advice: {:?}",
            ui.0
        );
        let completion_reviews =
            ui.0.iter()
                .filter(|event| event.starts_with("Coach review skipped"))
                .count();
        assert_eq!(
            completion_reviews,
            usize::from(outcome == "completed"),
            "{outcome}: {:?}",
            ui.0
        );
    }
}

#[test]
fn malformed_coach_review_does_not_loop_or_hide_original_reply() {
    let server = fake::serve(vec![sse_reply("Answer."), sse_reply("not JSON")]);
    let config = coached_config(&server.url);
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.set_advisor(true);
    let report = agent.turn("Answer the question", &mut NullUi).unwrap();
    assert_eq!(report.text, "Answer.");
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn remote_coach_is_not_contacted_without_disclosure_opt_in() {
    let server = fake::serve(vec![sse_reply("Answer.")]);
    let config = coached_config("https://example.com");
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.set_advisor(true);
    assert_eq!(
        agent.turn("Answer the question", &mut NullUi).unwrap().text,
        "Answer."
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn coach_respects_readonly_approval_without_contacting_backend() {
    let server = fake::serve(vec![sse_reply("Answer.")]);
    let config = coached_config(&server.url);
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.approval = crate::config::types::Approval::Readonly;
    agent.set_advisor(true);
    assert_eq!(
        agent.turn("Answer the question", &mut NullUi).unwrap().text,
        "Answer."
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn coach_disabled_and_needs_input_do_not_start_repairs() {
    for (enabled, reply) in [(false, "Answer."), (true, "Needs input: which file?")] {
        let server = fake::serve(vec![sse_reply(reply)]);
        let config = coached_config(&server.url);
        let dir = workspace();
        let mut agent = new_agent(&config, &server.url, dir.path());
        agent.set_advisor(enabled);
        assert_eq!(
            agent.turn("Inspect notes", &mut NullUi).unwrap().text,
            reply
        );
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn coach_repair_does_not_refund_the_turn_step_budget() {
    let server = fake::serve(vec![
        sse_reply("Done."),
        sse_reply(
            r#"{"gaps":[{"requirement":"inspect notes","evidence":0,"next_check":"read notes.txt"}]}"#,
        ),
    ]);
    let mut config = coached_config(&server.url);
    config.harness.max_steps = 1;
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.set_advisor(true);
    assert!(
        agent.turn("Inspect notes", &mut NullUi).is_err(),
        "hermetic step exhaustion must remain an error"
    );
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn automatic_coach_uses_observed_results_to_recover_during_turn() {
    let server = fake::serve(vec![
        sse_tool_call("read", json!({"path":"missing.txt"})),
        sse_reply("Try the existing notes.txt instead and inspect its contents."),
        sse_tool_call("read", json!({"path":"notes.txt"})),
        sse_reply("Notes contain alpha."),
    ]);
    let mut config = coached_config(&server.url);
    config.escalation.enabled = true;
    config.escalation.auto_after_failed_rounds = 1;
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    let report = agent.turn("Inspect notes", &mut NullUi).unwrap();
    assert_eq!(report.text, "Notes contain alpha.");
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(fake::body(&requests[1])["model"], "coach-test");
    assert!(messages(&requests[1]).contains("missing.txt"));
    assert!(messages(&requests[2]).contains("Try the existing notes.txt"));
}

#[test]
fn goal_runs_two_continuations_then_completes_through_the_goal_tool() {
    let server = fake::serve(vec![
        sse_reply("started"),
        sse_tool_call("read", json!({"path": "notes.txt"})),
        sse_reply("checked the file"),
        sse_tool_call("read", json!({"path": "missing.txt"})),
        sse_reply("checked the missing file too"),
        sse_tool_call("goal", json!({"op": "complete"})),
        sse_reply("done"),
    ]);
    let config = Config::default();
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent
        .goal_mutate(|goal| goal.start("ship the parser", None))
        .unwrap();

    let report = agent.turn("begin", &mut NullUi).unwrap();

    assert_eq!(report.text, "done");
    let goal = agent.modes().goal.goal.clone().unwrap();
    assert_eq!(goal.status, Status::Complete);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        7,
        "one user turn, three continuations, each with its follow-up reply"
    );
    let last = messages(requests.last().unwrap());
    assert_eq!(
        last.matches("Continue active goal.").count(),
        3,
        "every continuation is sent to the model"
    );
    let offered: Vec<String> = fake::body(&requests[1])["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .map(|x| x["function"]["name"].to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        offered.iter().any(|n| n == "\"goal\""),
        "goal tool is offered: {offered:?}"
    );
}

#[test]
fn goal_stops_with_budget_limit_message_when_the_budget_is_spent() {
    let server = fake::serve(vec![
        sse_reply_with_usage("working", 40, 10),
        sse_reply("wrapping up"),
    ]);
    let config = Config::default();
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent
        .goal_mutate(|goal| goal.start("ship the parser", Some(20)))
        .unwrap();

    agent.turn("begin", &mut NullUi).unwrap();

    let goal = agent.modes().goal.goal.clone().unwrap();
    assert_eq!(goal.status, Status::BudgetLimited);
    assert!(
        goal.tokens_used >= 20,
        "turn usage counted: {}",
        goal.tokens_used
    );
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one budget-limit turn, then the loop stops"
    );
    assert!(messages(requests.last().unwrap()).contains("Active goal token budget reached."));
}

#[test]
fn a_blocked_turn_stops_goal_continuation_but_an_incomplete_one_continues() {
    use crate::agent::TurnOutcome;
    // Script exactly the expected requests: the fake server waits for every scripted one, and an
    // unexpected extra request is refused once the scripts run out.
    for (reply, expected) in [
        ("Blocked: the API key is missing.", 1),
        ("Incomplete: still need the key", 2),
    ] {
        let mut scripts = vec![sse_reply(reply)];
        if expected == 2 {
            scripts.push(sse_reply("continuing"));
        }
        let server = fake::serve(scripts);
        let config = Config::default();
        let dir = workspace();
        let mut agent = new_agent(&config, &server.url, dir.path());
        agent
            .goal_mutate(|goal| goal.start("ship the parser", None))
            .unwrap();
        let report = agent.turn("begin", &mut NullUi).unwrap();
        assert_eq!(
            server.requests().len(),
            expected,
            "{reply}: a blocker waits for the human; an incomplete turn may continue"
        );
        if expected == 1 {
            assert_eq!(report.outcome, TurnOutcome::Blocked);
            assert_eq!(
                agent.modes().goal.goal.clone().unwrap().status,
                Status::Active
            );
        }
    }
}

#[test]
fn guided_goal_asks_questions_before_any_goal_exists() {
    let server = fake::serve(vec![
        sse_reply("What command proves success?"),
        sse_tool_call(
            "goal",
            json!({"op": "create", "objective": "## Objective\nship the parser\n## Success criteria\ncargo test passes"}),
        ),
        sse_reply("Goal created."),
        sse_tool_call("goal", json!({"op": "complete"})),
        sse_reply("done"),
    ]);
    let config = Config::default();
    let dir = workspace();
    let mut agent = new_agent(&config, &server.url, dir.path());
    agent.goal_mutate(|goal| goal.interview = true).unwrap();

    let question = agent
        .turn(&guided_kickoff("ship the parser"), &mut NullUi)
        .unwrap();

    assert_eq!(question.text, "What command proves success?");
    assert!(
        agent.modes().goal.goal.is_none(),
        "no goal before the interview finishes"
    );
    assert!(
        agent.modes().goal.interview,
        "the interview continues while no tool call is made"
    );

    agent.turn("cargo test passes", &mut NullUi).unwrap();

    let goal = agent.modes().goal.goal.clone().unwrap();
    assert_eq!(goal.status, Status::Complete);
    assert!(goal.objective.starts_with("## Objective"));
    assert!(!agent.modes().goal.interview);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        5,
        "question, create, follow-up, continuation that completes, final reply"
    );
    assert!(messages(&requests[0]).contains("ship the parser"));
}

#[test]
fn completed_goal_stops_counting_usage() {
    let mut state = State::default();
    state.start("ship the parser", None);
    state.account(10, 1);
    state.tool(&json!({"op": "complete"})).unwrap();
    state.account(500, 5);
    assert_eq!(state.goal.as_ref().unwrap().tokens_used, 10);
}

#[test]
fn interview_survives_grounding_tool_calls_and_ends_only_at_create() {
    let mut state = State {
        interview: true,
        ..Default::default()
    };
    let todo = TodoList::new();
    // A grounding read, then a user reply: the next request still offers `goal`.
    state.record(
        "read",
        &json!({"path": "Cargo.toml"}),
        &ToolOutput::ok("[package]".to_owned()),
    );
    assert!(state.follow_up(false, false, &mut None, &todo).is_none());
    assert!(state.interview);
    assert!(state.tool_offered());
    // A failed create keeps the interview open too.
    assert!(state.tool(&json!({"op": "create"})).is_err());
    assert!(state.tool_offered());
    // A successful create ends it.
    state
        .tool(&json!({"op": "create", "objective": "ship the parser"}))
        .unwrap();
    assert!(!state.interview);
}

#[test]
fn paused_goal_cannot_be_completed_without_resuming() {
    let mut state = State::default();
    state.start("ship the parser", None);
    state.pause().unwrap();
    assert!(state.tool(&json!({"op": "complete"})).is_err());
    assert_eq!(state.goal.as_ref().unwrap().status, Status::Paused);
    state.resume().unwrap();
    state.tool(&json!({"op": "complete"})).unwrap();
    assert_eq!(state.goal.as_ref().unwrap().status, Status::Complete);
}
