use super::*;

fn parsed(args: &str) -> Parsed {
    parse_args(args).unwrap_or_else(|error| panic!("{args:?} rejected: {error}"))
}

fn cond(command: &str, until: bool) -> Option<Condition> {
    Some(Condition {
        command: command.into(),
        until,
    })
}

#[test]
fn grammar_reads_counts_durations_flags_and_prompt() {
    assert_eq!(parsed(""), Parsed::default());
    assert_eq!(parsed("2").limit, Some(Limit::Iterations(2)));
    assert_eq!(parsed("keep going").prompt.as_deref(), Some("keep going"));
    assert_eq!(
        parsed("10m fix").limit,
        Some(Limit::Duration(Duration::from_secs(600)))
    );
    assert_eq!(
        parsed("10 minutes").limit,
        Some(Limit::Duration(Duration::from_secs(600)))
    );
    assert_eq!(
        parsed("1h30m").limit,
        Some(Limit::Duration(Duration::from_secs(5400)))
    );
    let until = parsed("20 --until 'bun test' fix the failing tests");
    assert_eq!(until.limit, Some(Limit::Iterations(20)));
    assert_eq!(until.condition, cond("bun test", true));
    assert_eq!(until.prompt.as_deref(), Some("fix the failing tests"));
    assert_eq!(parsed("--while=true").condition, cond("true", false));
}

#[test]
fn grammar_rejects_malformed_limits_and_flags() {
    for bad in ["-1", "1.5h", "10x10", "0"] {
        assert!(parse_args(bad).is_err(), "{bad} should be rejected");
    }
    assert!(
        parse_args("--until")
            .unwrap_err()
            .contains("needs a shell command")
    );
    assert!(
        parse_args("--until 'open")
            .unwrap_err()
            .contains("unterminated quote")
    );
    assert!(
        parse_args("--until a --while b")
            .unwrap_err()
            .contains("Use only one")
    );
    assert!(
        parse_args("--bogus x")
            .unwrap_err()
            .starts_with("Unknown /loop flag --bogus.")
    );
}

#[test]
fn enable_message_matches_omp_wording() {
    let mut mode = LoopMode::default();
    let message = mode.enable(&parsed("2 fix"), Instant::now());
    assert_eq!(
        message,
        "Loop mode enabled. Limited to 2 iterations. 2 of 2 iterations remaining. Repeating it after each turn. Esc suspends the ongoing loop; /loop again to disable."
    );
}

#[test]
fn budget_of_two_allows_exactly_two_turns() {
    let t0 = Instant::now();
    let mut mode = LoopMode::default();
    mode.enable(&parsed("2 go"), t0);
    mode.turn_started("go");
    mode.turn_settled(t0, false);
    let at = t0 + AUTO_SUBMIT_DELAY;
    assert_eq!(
        mode.take_due(at, |_| unreachable!()),
        Due::Submit("go".into())
    );
    mode.turn_started("go");
    mode.turn_settled(at, false);
    assert_eq!(
        mode.take_due(at + AUTO_SUBMIT_DELAY, |_| unreachable!()),
        Due::Stop(LIMIT_REACHED.into())
    );
    assert!(!mode.is_enabled());
}

#[test]
fn condition_verdict_stops_the_loop_early() {
    let t0 = Instant::now();
    let mut mode = LoopMode::default();
    mode.enable(&parsed("5 --until 'true' go"), t0);
    mode.turn_started("go");
    mode.turn_settled(t0, false);
    let due = t0 + AUTO_SUBMIT_DELAY;
    let stop = "Loop condition `true` is now satisfied. Loop mode disabled.";
    let verdict = settled(Verdict::Stop(stop.into()));
    assert_eq!(
        mode.take_due(due, |c| {
            assert!(c.until);
            verdict
        }),
        Due::Idle
    );
    assert_eq!(
        mode.take_due(due, |_| unreachable!()),
        Due::Stop(stop.into())
    );
    assert!(!mode.is_enabled());
}

#[test]
fn interrupt_pauses_and_a_manual_prompt_resumes() {
    let t0 = Instant::now();
    let mut mode = LoopMode::default();
    mode.enable(&parsed("go"), t0);
    mode.turn_started("go");
    mode.turn_settled(t0, true);
    assert_eq!(mode.next_due(), None);
    assert_eq!(
        mode.take_due(t0 + AUTO_SUBMIT_DELAY, |_| unreachable!()),
        Due::Idle
    );
    mode.turn_started("again");
    mode.turn_settled(t0, false);
    assert_eq!(
        mode.take_due(t0 + AUTO_SUBMIT_DELAY, |_| unreachable!()),
        Due::Submit("again".into())
    );
}

#[test]
fn evaluate_maps_exit_status_onto_verdicts() {
    let cwd = std::env::temp_dir();
    let wait = Duration::from_secs(10);
    let run = |command: &str, until: bool| {
        evaluate(
            &Condition {
                command: command.into(),
                until,
            },
            &cwd,
            || false,
            wait,
        )
    };
    assert_eq!(run("true", false), Verdict::Continue);
    assert_eq!(run("false", true), Verdict::Continue);
    assert_eq!(
        run("true", true),
        Verdict::Stop("Loop condition `true` is now satisfied. Loop mode disabled.".into())
    );
    assert_eq!(
        run("false", false),
        Verdict::Stop("Loop condition `false` no longer holds. Loop mode disabled.".into())
    );
    assert_eq!(
        run("echo broken; exit 2", false),
        Verdict::Stop(
            "Loop condition `echo broken; exit 2` failed (exit 2): broken. Loop mode disabled."
                .into()
        )
    );
    assert_eq!(
        evaluate(
            &Condition {
                command: "sleep 5".into(),
                until: false
            },
            &cwd,
            || true,
            wait
        ),
        Verdict::Aborted
    );
}

/// Runs one hermetic agent against scripted replies and returns the closure's result with the raw requests.
fn with_fake_agent<T>(
    scripts: Vec<crate::llm::fake::Script>,
    run: impl FnOnce(&mut crate::agent::Agent<'_>) -> T,
) -> (T, Vec<Vec<u8>>) {
    let config = crate::config::types::Config::default();
    let server = crate::llm::fake::serve(scripts);
    let backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let workspace = tempfile::tempdir().unwrap();
    let version = crate::config::types::Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let mut agent =
        crate::agent::Agent::hermetic(&config, backend, &version, workspace.path(), None, None)
            .unwrap();
    let output = run(&mut agent);
    drop(agent);
    (output, server.requests())
}

fn sse_reply(text: &str) -> crate::llm::fake::Script {
    let event = serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    crate::llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

/// A gate whose verdict is already known, standing in for a finished condition.
fn settled(verdict: Verdict) -> Gate {
    let (tx, verdicts) = mpsc::channel();
    tx.send(verdict).unwrap();
    Gate {
        command: String::new(),
        verdicts,
        stop: Arc::new(AtomicBool::new(false)),
    }
}

/// Mirrors the Shell's hook order: arm, run a turn, settle, then take the due iteration,
/// waiting out any condition that is still running.
fn run_loop<F: Fn(&Condition) -> Gate>(
    mode: &mut LoopMode,
    agent: &mut crate::agent::Agent<'_>,
    parsed: &Parsed,
    gate: F,
) -> Vec<String> {
    let mut notes = vec![mode.enable(parsed, Instant::now())];
    let mut prompt = parsed.prompt.clone().unwrap_or_default();
    let mut now = Instant::now();
    loop {
        mode.turn_started(&prompt);
        let report = agent.turn(&prompt, &mut crate::agent::NullUi).unwrap();
        mode.turn_settled(now, report.interrupted);
        now += Duration::from_secs(1);
        prompt = loop {
            match mode.take_due(now + Duration::from_secs(1), &gate) {
                Due::Submit(next) => break next,
                Due::Idle if mode.is_awaiting_condition() => {
                    thread::sleep(Duration::from_millis(5));
                }
                Due::Stop(message) | Due::Paused(message) => {
                    notes.push(message);
                    return notes;
                }
                Due::Idle => return notes,
            }
        };
    }
}

#[test]
fn a_limit_of_two_runs_exactly_two_turns_then_stops() {
    let parsed = parsed("2 keep going");
    let (notes, requests) = with_fake_agent(vec![sse_reply("one"), sse_reply("two")], |agent| {
        let mut mode = LoopMode::default();
        run_loop(&mut mode, agent, &parsed, |_| settled(Verdict::Continue))
    });
    assert_eq!(requests.len(), 2, "limit 2 must run exactly two turns");
    assert_eq!(notes.last().map(String::as_str), Some(LIMIT_REACHED));
}

#[test]
fn an_until_condition_that_holds_stops_the_loop_early() {
    let parsed = parsed("5 --until 'true' keep going");
    let ((notes, enabled), requests) = with_fake_agent(vec![sse_reply("one")], |agent| {
        let mut mode = LoopMode::default();
        let notes = run_loop(&mut mode, agent, &parsed, |condition| {
            Gate::start(
                condition,
                std::path::Path::new("."),
                Arc::new(AtomicBool::new(false)),
                CONDITION_TIMEOUT,
            )
        });
        (notes, mode.is_enabled())
    });
    assert_eq!(
        requests.len(),
        1,
        "a condition that holds after the first turn stops the loop"
    );
    assert!(!enabled);
    assert!(
        notes
            .last()
            .is_some_and(|message| message.contains("Loop mode disabled."))
    );
}

#[test]
fn a_cancelled_iteration_pauses_the_loop_and_reports_it() {
    let parsed = parsed("3 --until 'true' keep going");
    let ((notes, enabled, due), requests) = with_fake_agent(vec![sse_reply("one")], |agent| {
        let mut mode = LoopMode::default();
        let notes = run_loop(&mut mode, agent, &parsed, |_| settled(Verdict::Aborted));
        (notes, mode.is_enabled(), mode.next_due())
    });
    assert_eq!(
        requests.len(),
        1,
        "a cancelled iteration must not submit another turn"
    );
    assert!(enabled, "a paused loop stays enabled");
    assert!(due.is_none());
    assert_eq!(
        notes.last().map(String::as_str),
        Some("Loop paused. Send a prompt to resume.")
    );
}
