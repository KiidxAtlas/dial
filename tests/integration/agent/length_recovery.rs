//! Length recovery through the real foreground/child controllers and scripted HTTP transport.
use super::*;

fn length_script(partial_tool: bool) -> Vec<Reply> {
    let mut script = vec![Reply::Call(
        "write",
        json!({"path":"completed.txt","content":"completed effect"}),
    )];
    // More replies than bounded recovery permits, but a finite fixture also ensures a regression
    // fails an assertion rather than leaving the test running forever with an unlimited budget.
    script.extend((0..32).map(|_| {
        if partial_tool {
            // Even parseable arguments must not execute when the provider marks the call cut off.
            Reply::LengthCall(
                "write",
                json!({"path":"partial-leak.txt","content":"must never execute"}),
            )
        } else {
            Reply::Cut
        }
    }));
    script
}

fn unlimited_length_env(url: &str) -> Env {
    let mut e = env(url, "auto");
    e.config.harness.max_steps = 0;
    // Match the scripted model's configured output ceiling; recovery can grow it on retries.
    e.config.harness.max_tokens = 8192;
    e.config.reasoning_guard.enabled = false;
    e
}

fn assert_length_effects(e: &Env, server: &Scripted) {
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("completed.txt")).unwrap(),
        "completed effect"
    );
    assert!(!e.project.path().join("partial-leak.txt").exists());
    let requests = server.requests.lock();
    assert!(
        (9..=16).contains(&requests.len()),
        "length recovery must stop before the finite fixture is exhausted: {} requests",
        requests.len()
    );
    // Exactly the first, completed call is admitted. No cut-off call is published as a tool turn.
    let messages = requests.last().unwrap()["messages"].as_array().unwrap();
    assert_eq!(messages.iter().filter(|m| m["role"] == "tool").count(), 1);
}

#[test]
fn foreground_unlimited_steps_bounds_repeated_length_and_preserves_completed_effects() {
    for partial_tool in [false, true] {
        let server = serve(length_script(partial_tool));
        let mut e = unlimited_length_env(&server.url);
        e.config.harness.tools = vec!["write".into()];
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let mut ui = Recorder::default();
        let report = a.turn("Write the requested files.", &mut ui).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(
            report.text.contains("repeated output truncation"),
            "{}",
            report.text
        );
        assert_eq!(ui.calls, ["write"]);
        assert_length_effects(&e, &server);
    }
}

#[test]
fn child_unlimited_steps_bounds_repeated_length_and_preserves_completed_effects() {
    for partial_tool in [false, true] {
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"agent":"worker","task":"Write the requested files.","solution_space":"s","max_steps":0}),
            ),
            Reply::Text("The worker stopped incomplete; its completed effect is retained."),
        ]);
        let side = serve(length_script(partial_tool));
        let mut e = unlimited_length_env(&main.url);
        e.config.harness.tools = vec!["task".into(), "write".into()];
        with_subagent_role(&mut e, &side.url);
        run_turn(&e, &mut Recorder::default());
        let result = task_result(&main);
        assert!(result.contains("incomplete"), "{result}");
        assert!(result.contains("repeated output truncation"), "{result}");
        assert_eq!(main.requests.lock().len(), 2);
        assert_length_effects(&e, &side);
    }
}
