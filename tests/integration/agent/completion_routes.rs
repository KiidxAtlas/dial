//! Completion gates that differ by route: lean local routes keep the guard rails; hosted and
//! full-tier local routes (here `assistance.lean_local = false`) rely on the state gate alone.
use super::*;

fn route_env(url: &str, lean: bool) -> Env {
    let mut e = env(url, "auto");
    e.config.assistance.lean_local = if lean {
        dial::config::types::LeanLocal::On
    } else {
        dial::config::types::LeanLocal::Off
    };
    std::fs::write(e.project.path().join("parser.rs"), "fn parse() {}\n").unwrap();
    e
}

fn contains(requests: &[Value], needle: &str) -> usize {
    requests
        .iter()
        .filter(|request| request["messages"].to_string().contains(needle))
        .count()
}

#[test]
fn open_execution_steps_refuse_a_final_reply_on_every_route_and_are_never_closed_implicitly() {
    for lean in [false, true] {
        let server = serve(vec![
            Reply::Call(
                "todo",
                json!({"op":"set","tasks":[
                    {"content":"Summarize the parser"},
                    {"content":"Check error handling"},
                    {"content":"Maybe add docs","kind":"follow_up"}
                ]}),
            ),
            Reply::Text("The parser lives in parser.rs."),
            Reply::Text("The parser lives in parser.rs."),
        ]);
        let e = route_env(&server.url, lean);
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let mut ui = Recorder::default();
        let report = a.turn("Summarize the parser module", &mut ui).unwrap();
        assert_eq!(
            report.outcome,
            TurnOutcome::Incomplete,
            "lean={lean}: {}",
            report.text
        );
        assert!(
            report.text.contains(
                "Execution steps remain open: Summarize the parser; Check error handling"
            ),
            "lean={lean}: {}",
            report.text
        );
        assert!(
            !ui.notices.iter().any(|notice| notice.contains("Closed ")),
            "lean={lean}: {:?}",
            ui.notices
        );
        let todo = &a.session().control_state()["todo"];
        let open: Vec<_> = todo
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| matches!(task["status"].as_str(), Some("pending" | "in_progress")))
            .map(|task| task["content"].as_str().unwrap())
            .collect();
        assert_eq!(open.len(), 3, "lean={lean}: {todo}");
    }
}

#[test]
fn the_investigation_nudge_is_lean_only_and_never_ends_a_turn_incomplete() {
    let prompt = "Explain why the parser module exists";
    let nudge = "No investigation has been observed";
    // Non-lean: a tool-free explanation is accepted in one request.
    let server = serve(vec![Reply::Text("It turns tokens into a tree.")]);
    let e = route_env(&server.url, false);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn(prompt, &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert_eq!(server.requests.lock().len(), 1);

    // Lean: one nudge, then the repeated tool-free answer is accepted rather than refused.
    let server = serve(vec![
        Reply::Text("It turns tokens into a tree."),
        Reply::Text("It turns tokens into a tree."),
    ]);
    let e = route_env(&server.url, true);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn(prompt, &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(contains(&requests, nudge), 1);
}

#[test]
fn a_question_for_the_user_pauses_through_finish_when_offered_and_plain_otherwise() {
    let typed = serve(vec![Reply::Call(
        "finish",
        json!({"outcome":"needs_input","summary":"Need the target branch.","reason":"Which branch should I use?","options":["main","feature/parser"]}),
    )]);
    let e = route_env(&typed.url, false);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Update the parser", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::NeedsInput, "{}", report.text);
    assert!(report.text.contains("Which branch should I use?"));
    assert_eq!(
        typed.requests.lock().len(),
        1,
        "a typed pause needs no nudge"
    );

    for lean in [false, true] {
        let plain = serve(vec![
            Reply::Text("Which branch should I use?"),
            Reply::Text("Which branch should I use?"),
        ]);
        let e = route_env(&plain.url, lean);
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let report = a
            .turn("Update the parser", &mut Recorder::default())
            .unwrap();
        assert_eq!(
            report.outcome,
            TurnOutcome::NeedsInput,
            "lean={lean}: {}",
            report.text
        );
        // Hosted routes spend their one completion nudge on the typed pause; lean routes spend it on
        // the investigation nudge first, so a question is never accepted as Completed on either.
        let requests = plain.requests.lock();
        assert_eq!(
            contains(&requests, "call finish with outcome=needs_input"),
            usize::from(!lean),
            "lean={lean}"
        );
    }
}
