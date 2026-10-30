//! Turn-loop reliability: delivery of one-shot notes, bounded recovery and graceful stops.
use super::completion_tests::{completion_requests, connect, response};
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

fn mentions(request: &Value, needle: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
            message["content"]
                .as_str()
                .is_some_and(|text| text.contains(needle))
        })
}

fn unavailable() -> llm::fake::Script {
    llm::fake::json(503, &json!({"error":{"message":"overloaded"}}))
}

fn fallback_to(url: &str) -> String {
    format!(
        "[compaction]\nenabled=false\n[retry]\nmax_attempts=1\n[model_roles.coach]\nprovider='endpoint'\nmodel='fallback'\nbase_url='{url}/v1'\n[retry.fallback_chains]\ndefault=['@coach']\n"
    )
}

#[test]
fn a_new_turn_recovers_a_failed_stream_writer_without_replaying_the_failed_turn() {
    let server = llm::fake::serve(vec![
        response("This reply cannot be published.", None),
        response("Recovered.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let journal = agent.session.runtime().stream_writer().unwrap();
        let path = crate::runtime::stream_journal::path(agent.session.path().unwrap());
        journal.fail_writes_for_test(std::fs::File::open(path).unwrap());

        let error = agent
            .turn("Do not replay this request", &mut NullUi)
            .err()
            .expect("the first turn must stop on the failed journal write");
        assert!(
            error.to_string().contains("stream journal failed"),
            "{error:#}"
        );
        assert!(journal.check().is_err(), "the write failure must be sticky");

        let report = agent.turn("Answer the new request", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert_eq!(report.text, "Recovered.");
        assert!(journal.check().is_ok(), "admission repairs the same writer");
    });
    let requests = completion_requests(&server);
    assert_eq!(
        requests.len(),
        2,
        "one failed request and one explicitly started new turn"
    );
    assert!(mentions(&requests[1], "Answer the new request"));
}

#[test]
fn one_shot_notes_survive_a_request_discarded_before_any_reply() {
    let server = llm::fake::serve(vec![
        unavailable(),
        response("Done.", None),
        llm::fake::json(200, &json!({"total_slots":1})),
    ]);
    with_agent(&fallback_to(&server.url), |agent| {
        connect(agent, &server.url);
        agent
            .background_notices
            .push("Background job 7 finished.".into());
        let report = agent.turn("Say done", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    assert!(mentions(&requests[0], "Background job 7 finished."));
    assert!(
        mentions(&requests[1], "Background job 7 finished."),
        "the retried request must carry the undelivered notice"
    );
}

#[test]
fn a_ready_fallback_takes_over_after_one_retry_instead_of_every_attempt() {
    let server = llm::fake::serve(vec![
        unavailable(),
        unavailable(),
        response("Done.", None),
        slots(),
    ]);
    let config = fallback_to(&server.url).replace("max_attempts=1", "max_attempts=6");
    with_agent(&config, |agent| {
        connect(agent, &server.url);
        agent.backend.as_mut().unwrap().retry_backoff = std::time::Duration::from_millis(1);
        let report = agent.turn("Say done", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3);
    assert_ne!(requests[1]["model"], "fallback", "one retry on the primary");
    assert_eq!(
        requests[2]["model"], "fallback",
        "the third attempt goes to the fallback, not the primary's remaining retries"
    );
}

fn overflow() -> llm::fake::Script {
    llm::fake::json(
        400,
        &json!({"error":{"message":"This model's maximum context length is 8192 tokens. However, your messages resulted in 9000 tokens.","code":"context_length_exceeded"}}),
    )
}

#[test]
fn an_overflow_that_compaction_cannot_fix_stops_the_turn_instead_of_failing_it() {
    let server = llm::fake::serve(vec![
        overflow(),
        overflow(),
        overflow(),
        overflow(),
        overflow(),
        overflow(),
        overflow(),
        overflow(),
        llm::fake::json(200, &json!({"total_slots":1})),
    ]);
    let result = with_agent("[retry]\nmax_attempts=1\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Say done", &mut NullUi)
    });
    let report = result.expect("a context overflow must end the turn gracefully, not as an error");
    assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
    assert!(
        report.text.contains("compaction could not make room"),
        "{}",
        report.text
    );
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

#[test]
fn a_deadline_passing_during_a_tool_round_skips_the_unstarted_calls_and_stops() {
    let server = llm::fake::serve(vec![
        calls(&[
            ("check", json!({"names":["slow"]})),
            ("write", json!({"path":"after","content":"x\n"})),
        ]),
        llm::fake::json(200, &json!({"total_slots":1})),
    ]);
    let overlay = "[compaction]\nenabled=false\n[[verification.commands]]\nname=\"slow\"\ncommand=\"sleep 2\"\ntimeout_secs=10\n";
    let (report, wrote) = with_agent(overlay, |agent| {
        connect(agent, &server.url);
        agent.modes.launch.deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        let report = agent.turn("Check, then write", &mut NullUi).unwrap();
        (report, agent.workspace.root().join("after").exists())
    });
    assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
    assert!(report.text.contains("--max-time"), "{}", report.text);
    assert!(!wrote, "the call after the deadline must not run");
    assert_eq!(
        completion_requests(&server).len(),
        1,
        "no further request after the deadline"
    );
}

/// Interrupts only once both parallel reads have visibly completed, before the write can run.
struct InterruptAfterParallelReads {
    interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    finished_reads: usize,
}

impl Ui for InterruptAfterParallelReads {
    fn tool_result_with_id(&mut self, _call_id: &str, name: &str, _output: &ToolOutput) {
        if name == "read" {
            self.finished_reads += 1;
            if self.finished_reads == 2 {
                self.interrupt.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[test]
fn an_interrupt_keeps_finished_parallel_reads_and_marks_the_rest_not_run() {
    let server = llm::fake::serve(vec![
        calls(&[
            ("read", json!({"path":"a.txt"})),
            ("read", json!({"path":"b.txt"})),
            ("write", json!({"path":"after.txt","content":"x\n"})),
        ]),
        slots(),
    ]);
    let (report, results, wrote) = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        for (name, text) in [("a.txt", "alpha\n"), ("b.txt", "bravo\n")] {
            std::fs::write(agent.workspace.root().join(name), text).unwrap();
        }
        // A first result does not imply the sibling has started, even for tiny files.
        // Wait for observable completion of both reads rather than racing their workers.
        let mut ui = InterruptAfterParallelReads {
            interrupt: agent.interrupt.clone(),
            finished_reads: 0,
        };
        let report = agent.turn("Read both, then write", &mut ui).unwrap();
        assert!(agent.session.custom_records("training_turn").is_empty());
        let results: Vec<Value> = agent
            .session
            .messages()
            .into_iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        (
            report,
            results,
            agent.workspace.root().join("after.txt").exists(),
        )
    });
    assert_eq!(report.outcome, TurnOutcome::Interrupted, "{}", report.text);
    assert!(!wrote, "a call after the interrupt must not run");
    assert_eq!(results.len(), 3, "every call keeps exactly one result");
    let text = |index: usize| results[index]["content"].as_str().unwrap_or_default();
    assert!(text(0).contains("alpha"), "{}", text(0));
    assert!(
        text(1).contains("bravo"),
        "a read that already finished keeps its actual result: {}",
        text(1)
    );
    assert!(text(2).contains("interrupted"), "{}", text(2));
    for result in &results {
        assert_ne!(
            result["dial_effects"], "unknown",
            "no call here has unknown effects: {result}"
        );
    }
}

#[test]
fn a_deadline_passing_during_generation_cancels_it_and_stops() {
    let server = llm::fake::serve(vec![llm::fake::Script::Drip {
        pause: std::time::Duration::from_millis(200),
        count: 100,
    }]);
    let started = std::time::Instant::now();
    let report = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.modes.launch.deadline =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        agent.turn("Say done", &mut NullUi).unwrap()
    });
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the generation must end near the deadline"
    );
    assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
    assert!(report.text.contains("--max-time"), "{}", report.text);
}

#[test]
fn request_timeouts_are_clamped_to_the_time_left() {
    use super::begin::clamp_to_deadline;
    use std::time::Duration;
    assert_eq!(clamp_to_deadline(600, None), 600);
    assert_eq!(clamp_to_deadline(0, None), 0);
    assert_eq!(clamp_to_deadline(600, Some(Duration::from_millis(2500))), 3);
    assert_eq!(clamp_to_deadline(0, Some(Duration::from_secs(40))), 40);
    assert_eq!(clamp_to_deadline(5, Some(Duration::from_secs(40))), 5);
    assert_eq!(clamp_to_deadline(600, Some(Duration::ZERO)), 1);
}

#[test]
fn the_portable_guidance_regeneration_limit_stops_the_turn_instead_of_failing_it() {
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, "http://127.0.0.1:9");
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 0, "goal", &mut NullUi)
            .unwrap();
        turn.rule_regenerations = agent.config.portable_rules.max_regenerations;
        let tick = agent
            .regenerate_for_guidance(&mut turn, 0, None, None, None, &mut NullUi)
            .expect("the limit must not surface as an error");
        let crate::agent::run::engine::Tick::Finished(report) = tick else {
            panic!("the limit must end the turn");
        };
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(report.text.contains("portable guidance"), "{}", report.text);
        assert!(report.text.contains("continue"), "{}", report.text);
    });
}

#[test]
fn a_resumed_round_keeps_the_turn_and_counts_only_its_own_steps() {
    let server = llm::fake::serve(vec![
        response("First answer.", None),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("Second answer.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut schemas = agent.schemas(&names).unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 5, "goal", &mut NullUi)
            .unwrap();
        let mut engine = crate::agent::run::engine::Engine::default();
        let (first, consumed) = agent
            .run_round(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                5,
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(first.outcome, TurnOutcome::Completed, "{}", first.text);
        assert_eq!((first.steps, consumed), (1, 1));

        // State the first round built up: turn-wide facts survive, its recovery rounds do not.
        turn.solo_todo_noted = true;
        turn.reviewed_paths.insert("src/lib.rs".into());
        turn.auto_continues = 2;
        agent
            .push(json!({"role":"user","content":"A stop hook blocked completion."}))
            .unwrap();
        let (second, consumed) = agent
            .resume_round(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                5,
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(second.outcome, TurnOutcome::Completed, "{}", second.text);
        assert_eq!(
            (second.steps, consumed),
            (1, 1),
            "a resumed round reports its own steps"
        );
        assert_eq!(
            turn.iteration, 2,
            "step indices keep counting across rounds"
        );
        assert!(turn.solo_todo_noted);
        assert!(turn.reviewed_paths.contains("src/lib.rs"));
        assert_eq!(turn.auto_continues, 0);
        agent.backend = Some(backend);
    });
    assert_eq!(completion_requests(&server).len(), 2);
}

const NOTE_CHECK: &str = "[compaction]\nenabled=false\n[[verification.commands]]\nname=\"notes\"\ncommand=\"test -f note.txt\"\nfile_extensions=[\"txt\"]\ntimeout_secs=5\n";

/// The endpoint's slot probe (`GET /props`) follows the first request; a valid answer is cached.
fn slots() -> llm::fake::Script {
    llm::fake::json(200, &json!({"total_slots":1}))
}

#[test]
fn an_edit_with_no_passing_check_is_reminded_once_then_disclosed() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("write", json!({"path":"note.txt","content":"x\n"}))),
        ),
        slots(),
        response("The note is written.", None),
        response("The note is written; I did not run the check.", None),
    ]);
    let report = with_agent(NOTE_CHECK, |agent| {
        connect(agent, &server.url);
        agent.turn("Write the note", &mut NullUi).unwrap()
    });
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert!(
        report.text.contains("Unverified") && report.text.contains("notes"),
        "the accepted answer discloses the missing evidence: {}",
        report.text
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3, "exactly one reminder round");
    assert!(mentions(&requests[2], "no relevant check has passed"));
    assert!(!mentions(&requests[1], "no relevant check has passed"));
}

#[test]
fn a_passing_check_after_the_edit_satisfies_the_evidence_gate() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("write", json!({"path":"note.txt","content":"x\n"}))),
        ),
        slots(),
        response("", Some(("check", json!({"names":["notes"]})))),
        response("The note is written and its check passed.", None),
    ]);
    let report = with_agent(NOTE_CHECK, |agent| {
        connect(agent, &server.url);
        agent.turn("Write the note", &mut NullUi).unwrap()
    });
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert!(!report.text.contains("Unverified"), "{}", report.text);
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3, "no reminder round");
    assert!(
        !requests
            .iter()
            .any(|request| mentions(request, "no relevant check has passed"))
    );
}

/// A tool call written as text, which the turn rejects as unusable.
fn text_call() -> llm::fake::Script {
    response(r#"{"name":"read","arguments":{"path":"src/lib.rs"}}"#, None)
}

#[test]
fn alternating_recoveries_without_progress_are_redirected_then_stopped() {
    // Each unusable reply is followed by a failing tool call, which resets the unusable-reply
    // cap without being progress. Only the shared ledger sees the loop.
    let mut scripts = vec![text_call(), slots()];
    for index in 0..15 {
        scripts.push(response(
            "",
            Some(("read", json!({"path":format!("missing-{index}.txt")}))),
        ));
        scripts.push(text_call());
    }
    let server = llm::fake::serve(scripts);
    let report = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Read the file", &mut NullUi).unwrap()
    });
    assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
    assert!(
        report.text.contains("16 recovery attempts in a row")
            && report.text.contains("unusable tool call ×16"),
        "{}",
        report.text
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 31, "no request after the ladder stops");
    assert!(
        requests
            .iter()
            .any(|request| mentions(request, "6 recovery attempts in a row")),
        "the redirecting note reaches the model"
    );
}

#[test]
fn the_last_step_of_a_round_asks_for_an_answer_and_runs_no_tools() {
    let server = llm::fake::serve(vec![
        response("", Some(("write", json!({"path":"first","content":"x\n"})))),
        slots(),
        response(
            "Wrote `first`; `second` is still missing.",
            Some(("write", json!({"path":"second","content":"x\n"}))),
        ),
    ]);
    let (report, wrote) = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut schemas = agent.schemas(&names).unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 2, "goal", &mut NullUi)
            .unwrap();
        let mut engine = crate::agent::run::engine::Engine::default();
        let (report, _) = agent
            .run_round(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                2,
                &mut NullUi,
            )
            .unwrap();
        let root = agent.workspace.root();
        (
            report,
            (root.join("first").exists(), root.join("second").exists()),
        )
    });
    assert_eq!(
        wrote,
        (true, false),
        "the final step's tool call must not run"
    );
    assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
    assert!(
        report.text.starts_with("Wrote `first`") && report.text.contains("step limit"),
        "the model's wrap-up is kept: {}",
        report.text
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    assert!(!mentions(&requests[0], "final request of this turn"));
    assert!(mentions(&requests[1], "final request of this turn"));
}

#[test]
fn most_of_the_time_budget_used_asks_once_to_prioritize_finishing() {
    let server = llm::fake::serve(vec![
        response("", Some(("write", json!({"path":"first","content":"x\n"})))),
        slots(),
        response("Done.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut schemas = agent.schemas(&names).unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 0, "goal", &mut NullUi)
            .unwrap();
        // 50 of the turn's 60 seconds are used: past 80%, with time left for both requests.
        let now = std::time::Instant::now();
        turn.turn_started = now - std::time::Duration::from_secs(50);
        agent.modes.launch.deadline = Some(now + std::time::Duration::from_secs(10));
        let mut engine = crate::agent::run::engine::Engine::default();
        let (report, _) = agent
            .run_round(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                0,
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    assert!(mentions(
        &requests[0],
        "About 80% of this turn's time limit"
    ));
    assert!(
        !mentions(&requests[1], "About 80%"),
        "the note is delivered once"
    );
}

#[test]
fn a_request_narrowed_by_the_planning_gate_leaves_the_turn_offer_intact() {
    let server = llm::fake::serve(vec![response("Already answered.", None)]);
    let kept = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut schemas = agent.schemas(&names).unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 5, "goal", &mut NullUi)
            .unwrap();
        turn.plan_after_gate = true;
        let mut engine = crate::agent::run::engine::Engine::default();
        let (report, _) = agent
            .run_round(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                5,
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        agent.backend = Some(backend);
        schemas.len() == names.len()
    });
    assert!(
        kept,
        "the gate narrows only its own request, not the turn's offer"
    );
    let requests = completion_requests(&server);
    let offered: std::collections::BTreeSet<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(offered, ["finish".to_owned(), "todo".to_owned()].into());
}

#[test]
fn a_failed_tool_cannot_execute_an_unchanged_retry_but_can_use_corrected_arguments() {
    use crate::runtime::tool_output::ToolOutput;
    use crate::tools::integrations::ext::{Tool, ToolContext};
    struct Probe(Arc<std::sync::atomic::AtomicUsize>);
    impl Tool for Probe {
        fn name(&self) -> &str {
            "retry_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"retry_probe","description":"Test probe", "parameters":{"type":"object","properties":{"fixed":{"type":"boolean"}},"required":["fixed"]}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
            self.0.fetch_add(1, Ordering::SeqCst);
            if args["fixed"] == true {
                ToolOutput::ok("corrected attempt succeeded".into())
            } else {
                ToolOutput::err("page.evaluate: Error: real CSS flag animation")
            }
        }
    }
    let server = llm::fake::serve(vec![
        response("", Some(("retry_probe", json!({"fixed":false})))),
        slots(),
        response("", Some(("retry_probe", json!({"fixed":false})))),
        response("", Some(("retry_probe", json!({"fixed":true})))),
        response("The corrected probe passed.", None),
    ]);
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    with_agent("[compaction]\nenabled=false", |agent| {
        connect(agent, &server.url);
        agent.ext.register(Arc::new(Probe(executed.clone())));
        agent.activate_tools(&["retry_probe".into()]).unwrap();
        let report = agent.turn(
            "Run the probe and correct any failed assertion.",
            &mut NullUi,
        );
        assert_eq!(report.unwrap().outcome, TurnOutcome::Completed);
    });
    assert_eq!(
        executed.load(Ordering::SeqCst),
        2,
        "the identical failed retry must never reach the tool"
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 4);
    let tool_error = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .unwrap()["is_error"]
            .as_bool()
    };
    assert_eq!(tool_error(&requests[2]), Some(true));
    assert_eq!(tool_error(&requests[3]), Some(false));
}

#[test]
fn file_handles_are_projected_for_the_model_but_expanded_for_approval_and_ui() {
    #[derive(Default)]
    struct View {
        calls: Vec<(String, Value)>,
        approvals: Vec<(String, String)>,
    }
    impl Ui for View {
        fn tool_call_with_args(&mut self, _: &str, name: &str, _: &str, args: &Value) {
            self.calls.push((name.to_owned(), args.clone()));
        }
        fn approve(&mut self, name: &str, summary: &str) -> Decision {
            self.approvals.push((name.to_owned(), summary.to_owned()));
            Decision::Allow
        }
    }
    let path = "deep/project/module/note.txt";
    let initial = "before\nunchanged\n";
    let tag = crate::tools::hashline::tag_of(initial);
    let tool = |id: &str, name: &str, args: Value| {
        let event = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
        llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
    };
    let server = llm::fake::serve(vec![
        tool("read-file", "read", json!({"path":path})),
        slots(),
        tool(
            "edit-file",
            "edit",
            json!({"input":format!("[@f1#{tag}]\nPUT 1.=1:\n+after\n")}),
        ),
        response("Done.", None),
    ]);
    with_agent(
        "approval='ask'\n[compaction]\nenabled=false\n[verification]\nenabled=false\n[checkpoint]\nenabled=false\n[harness]\nedit_mode='hashline'\n",
        |agent| {
            connect(agent, &server.url);
            std::fs::create_dir_all(agent.workspace.root().join("deep/project/module")).unwrap();
            std::fs::write(agent.workspace.root().join(path), initial).unwrap();
            let mut ui = View::default();
            let report = agent
                .turn("Change before to after in the note", &mut ui)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            assert_eq!(
                std::fs::read(agent.workspace.root().join(path)).unwrap(),
                b"after\nunchanged\n",
                "requests: {:?}; tool evidence: {:?}",
                server
                    .requests()
                    .iter()
                    .map(|r| String::from_utf8_lossy(r)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned())
                    .collect::<Vec<_>>(),
                agent
                    .session
                    .messages()
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .map(|m| &m["content"])
                    .collect::<Vec<_>>()
            );
            assert!(ui.calls.iter().any(|(name, args)| {
                name == "edit"
                    && args["input"]
                        .as_str()
                        .is_some_and(|input| input.contains(path) && !input.contains("@f1"))
            }));
            assert!(
                ui.approvals
                    .iter()
                    .any(|(name, summary)| name == "edit" && summary.contains(path)),
                "approval must expose the real target: {:?}",
                ui.approvals
            );
            let history = agent.session.messages();
            assert!(
                history
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .any(|m| m["content"]
                        .as_str()
                        .is_some_and(|body| body.contains(path))),
                "stored evidence retains real paths"
            );
        },
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|body| body.contains(&format!("[@f1#{tag}]"))))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["messages"].to_string().contains("dial_model_")),
        "private projections are not wire fields"
    );
}
