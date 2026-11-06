//! Behavioral regression for progress-aware automatic repair, driven through the real turn loop
//! against the deterministic scripted backend. No live model, network or sleeps are involved.
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled as with_agent;
use crate::agent::*;

const COMMAND: &str = "grep -q ok input || { echo \"AssertionError: input is not ok\"; exit 1; }";

fn check_config() -> String {
    format!(
        "[compaction]\nenabled=false\n[verification]\nautomatic_checks=true\n[[verification.commands]]\nname=\"ready\"\ncommand='{COMMAND}'\ntimeout_secs=5\n[verification.inputs.ready]\npaths=[\"input\"]\n[verification.requirements.delivery]\nchecks=[\"ready\"]\n"
    )
}

fn calls(calls: &[(&str, Value)]) -> llm::fake::Script {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, args))| json!({"index":index,"id":format!("call-{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}))
        .collect();
    let event = json!({"choices":[{"delta":{"content":"","tool_calls":calls},"finish_reason":"tool_calls"}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn text(reply: &str) -> llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":reply},"finish_reason":"stop"}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

fn slots() -> llm::fake::Script {
    llm::fake::json(200, &json!({"total_slots":1}))
}

fn connect(agent: &mut Agent<'_>, url: &str) {
    agent.backend = Some(Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    ));
}

/// Completion requests only: the slot probe is a json reply without `messages`.
fn completion_requests(server: &llm::fake::Fake) -> usize {
    server
        .requests()
        .iter()
        .filter_map(|request| {
            let body = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
            serde_json::from_slice::<Value>(&request[body + 4..]).ok()
        })
        .filter(|request| request["messages"].is_array())
        .count()
}

#[test]
fn a_repair_that_changes_no_declared_input_stops_early_with_the_failed_outcome() {
    let server = llm::fake::serve(vec![
        // The turn's write makes a mutation so the focused check runs.
        calls(&[("write", json!({"path":"note.txt","content":"x\n"}))]),
        slots(),
        text("Wrote the note."),
        // The repair round claims a fix but leaves the declared input untouched.
        text("I reviewed it and it should be fine."),
    ]);
    with_agent(&check_config(), |agent| {
        agent.approval = crate::config::types::Approval::Auto;
        connect(agent, &server.url);
        std::fs::write(agent.workspace.root().join("input"), "bad\n").unwrap();
        let report = agent.turn("Write the note", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Failed, "{}", report.text);
        assert!(
            report.text.contains("same confirmed diagnosis"),
            "{}",
            report.text
        );
        assert!(report.text.contains("input is not ok"), "{}", report.text);
        assert_eq!(agent.metrics.repair_attempts, 1, "stopped after one round");
        assert_eq!(
            agent.step_checks.evidence.latest_status(COMMAND),
            Some(crate::agent::verify::verification::CheckStatus::CodeFailed),
            "the failed check remains the last observed status"
        );
        assert!(agent.session.custom_records("training_turn").is_empty());
    });
    assert_eq!(completion_requests(&server), 3);
}

#[test]
fn a_repair_that_changes_a_declared_input_gets_another_run_and_passes() {
    let server = llm::fake::serve(vec![
        calls(&[("write", json!({"path":"note.txt","content":"x\n"}))]),
        slots(),
        text("Wrote the note."),
        // The repair reads the declared input first (the write tool requires a read witness),
        // then edits it, so the rerun is a real change rather than a stall.
        calls(&[("read", json!({"path":"input"}))]),
        calls(&[("write", json!({"path":"input","content":"ok\n"}))]),
        text("Fixed the input."),
    ]);
    with_agent(&check_config(), |agent| {
        agent.approval = crate::config::types::Approval::Auto;
        connect(agent, &server.url);
        std::fs::write(agent.workspace.root().join("input"), "bad\n").unwrap();
        let report = agent.turn("Write the note", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert_eq!(
            std::fs::read_to_string(agent.workspace.root().join("input")).unwrap(),
            "ok\n"
        );
        assert_eq!(agent.metrics.repair_attempts, 1);
    });
    assert_eq!(completion_requests(&server), 5);
}

#[test]
fn a_repair_that_changes_then_reverts_the_declared_input_stops_early() {
    let server = llm::fake::serve(vec![
        calls(&[("write", json!({"path":"note.txt","content":"x\n"}))]),
        slots(),
        text("Wrote the note."),
        // Actual edits advance the effect epoch, but restore the declared bytes before rerunning.
        calls(&[("read", json!({"path":"input"}))]),
        calls(&[(
            "write",
            json!({"path":"input","content":"different bad input\n"}),
        )]),
        calls(&[("read", json!({"path":"input"}))]),
        calls(&[("write", json!({"path":"input","content":"bad\n"}))]),
        text("Reverted the input."),
    ]);
    with_agent(&check_config(), |agent| {
        agent.approval = crate::config::types::Approval::Auto;
        connect(agent, &server.url);
        std::fs::write(agent.workspace.root().join("input"), "bad\n").unwrap();
        let report = agent.turn("Write the note", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Failed, "{}", report.text);
        assert!(
            report.text.contains("same confirmed diagnosis"),
            "{}",
            report.text
        );
        assert_eq!(agent.metrics.repair_attempts, 1, "stopped after one round");
        assert!(
            agent.step_checks.mutations >= 3,
            "both repair writes must count as mutations"
        );
        assert_eq!(
            std::fs::read_to_string(agent.workspace.root().join("input")).unwrap(),
            "bad\n"
        );
    });
    assert_eq!(completion_requests(&server), 7);
}
