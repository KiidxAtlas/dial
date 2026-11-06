//! Turn-loop reliability: delivery of one-shot notes, bounded recovery and graceful stops.
use super::completion_tests::{completion_requests, connect, response};
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled as with_agent;

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
            .expect_err("the first turn must stop on the failed journal write");
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
fn recovery_labels_do_not_invent_failed_rounds_and_shell_success_clears_them() {
    // Missing reads are distinct real failures; bookkeeping between them is not work,
    // but must not manufacture another failed round from the retained phase label.
    let server = llm::fake::serve(vec![
        calls(&[("read", json!({"path":"missing-one"}))]),
        slots(),
        calls(&[("working_state", json!({"action":"view"}))]),
        calls(&[("read", json!({"path":"missing-two"}))]),
        calls(&[("bash", json!({"command":"printf 'recovered\\n'"}))]),
        response("Observed the shell result.", None),
    ]);
    with_agent(
        "[compaction]\nenabled=false\n[harness]\ntools=['read','bash','working_state']\n",
        |agent| {
            connect(agent, &server.url);
            let report = agent
                .turn("Inspect available evidence", &mut NullUi)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            let control = agent.session.control_state();
            let initial_phase = control["adaptive_history"][0]["selection"]["phase"]
                .as_str()
                .unwrap();
            let rounds: Vec<_> = control["adaptive_history"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|policy| {
                    let result = &policy["result"];
                    Some((
                        result["tool_round_failed"].as_bool()?,
                        result["next_observed_phase"].as_str()?,
                    ))
                })
                .collect();
            assert_eq!(
                rounds,
                vec![
                    (true, "recovery"),
                    (false, "recovery"),
                    (true, "recovery"),
                    (false, initial_phase)
                ]
            );
        },
    );
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
        let mut engine = crate::agent::run::engine::Engine::default();
        let tick = agent
            .regenerate_for_guidance(&mut turn, &mut engine, 0, (None, None), None, &mut NullUi)
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
fn a_write_and_known_check_in_one_response_run_in_order() {
    let server = llm::fake::serve(vec![
        calls(&[
            ("write", json!({"path":"note.txt","content":"ready\n"})),
            ("bash", json!({"command":"test -f note.txt"})),
        ]),
        slots(),
        response("The note is ready.", None),
    ]);
    with_agent(NOTE_CHECK, |agent| {
        connect(agent, &server.url);
        let report = agent.turn("Write and check the note", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert_eq!(
            std::fs::read_to_string(agent.workspace.root().join("note.txt")).unwrap(),
            "ready\n"
        );
        assert_eq!(
            agent.step_checks.evidence.latest_status("test -f note.txt"),
            Some(crate::agent::verify::verification::CheckStatus::Passed)
        );
    });
    assert_eq!(completion_requests(&server).len(), 2);
}

#[test]
fn a_native_write_refusal_skips_dependent_work_but_keeps_completed_reads_and_disjoint_writes() {
    let server = llm::fake::serve(vec![
        calls(&[
            (
                "write",
                json!({"path":"source.txt","content":"unseen overwrite\n"}),
            ),
            ("read", json!({"path":"evidence.txt"})),
            (
                "write",
                json!({"path":"source.txt","content":"dependent overwrite\n"}),
            ),
            ("bash", json!({"command":"printf ran > run-started.txt"})),
            ("check", json!({"names":["candidate"]})),
            (
                "write",
                json!({"path":"independent.txt","content":"independent change\n"}),
            ),
            (
                "write",
                json!({"path":"source.txt","content":"must not join the disjoint wave\n"}),
            ),
            ("read", json!({"path":"source.txt"})),
        ]),
        slots(),
        response(
            "The source overwrite was refused; independent work is retained.",
            None,
        ),
    ]);
    with_agent(
        "approval='auto'\n[compaction]\nenabled=false\n[verification]\nenabled=false\n[[verification.commands]]\nname='candidate'\ncommand='printf ran > check-started.txt'\n[edit]\nenforce_seen_lines=true\n[harness]\nedit_mode='hashline'\n[experimental]\ndependency_dispatch=true\n[task]\nmax_concurrency=4\n",
        |agent| {
            connect(agent, &server.url);
            agent.activate_tools(&["check".into()]).unwrap();
            let root = agent.workspace.root().to_path_buf();
            std::fs::write(root.join("source.txt"), "original source\n").unwrap();
            std::fs::write(root.join("evidence.txt"), "completed sibling evidence\n").unwrap();
            let report = agent
                .turn(
                    "Attempt the batch, retaining diagnostic evidence",
                    &mut NullUi,
                )
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            let results: Vec<Value> = agent
                .session
                .messages()
                .into_iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            assert_eq!(results.len(), 8, "{results:?}");
            assert_eq!(
                results[0]["is_error"], true,
                "native overwrite must be refused: {results:?}"
            );
            assert_ne!(results[0]["dial_execution_status"], "skipped");
            assert!(
                !agent.edit_preview_intercepted,
                "write must reach the native validator, not preview interception"
            );
            for index in [2, 3, 4, 6] {
                assert_eq!(
                    results[index]["dial_execution_status"], "skipped",
                    "{results:?}"
                );
                assert_eq!(results[index]["dial_effects"], "none", "{results:?}");
            }
            for (index, evidence) in [(1, "completed sibling evidence"), (7, "original source")] {
                assert_eq!(results[index]["is_error"], false, "{results:?}");
                assert!(
                    results[index]["content"]
                        .as_str()
                        .unwrap()
                        .contains(evidence),
                    "{results:?}"
                );
            }
            assert_eq!(results[5]["is_error"], false, "{results:?}");
            assert_eq!(
                std::fs::read_to_string(root.join("source.txt")).unwrap(),
                "original source\n"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("independent.txt")).unwrap(),
                "independent change\n"
            );
            for marker in ["run-started.txt", "check-started.txt"] {
                assert!(
                    !root.join(marker).exists(),
                    "skipped action executed: {marker}"
                );
            }
        },
    );
    assert_eq!(completion_requests(&server).len(), 2);
}

#[test]
fn an_observational_native_edit_does_not_advance_the_mutation_epoch() {
    let server = llm::fake::serve(vec![
        calls(&[
            ("read", json!({"path":"unchanged.txt"})),
            (
                "edit",
                json!({"path":"unchanged.txt","old_string":"same source","new_string":"same source"}),
            ),
        ]),
        slots(),
        response("The source already has the requested content.", None),
    ]);
    with_agent(
        "approval='auto'\n[compaction]\nenabled=false\n[verification]\nenabled=false\n[harness]\nedit_mode='replace'\n[edit]\nstreaming_abort=false\n",
        |agent| {
            connect(agent, &server.url);
            std::fs::write(
                agent.workspace.root().join("unchanged.txt"),
                "same source\n",
            )
            .unwrap();
            let report = agent
                .turn("Keep the requested content", &mut NullUi)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            let results: Vec<Value> = agent
                .session
                .messages()
                .into_iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            assert_eq!(results.len(), 2, "{results:?}");
            assert_eq!(results[1]["is_error"], false, "{results:?}");
            assert_eq!(results[1]["dial_effects"], "none", "{results:?}");
            assert_eq!(
                agent.step_checks.mutations, 0,
                "a successful no-op cannot stale check evidence"
            );
        },
    );
}

fn tool_results(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn repository_git_is_offered_without_loading_but_pinned_selections_stay_pinned() {
    with_agent("[assistance]\nlean_local=false\n", |agent| {
        assert!(
            agent
                .workspace
                .root()
                .ancestors()
                .any(|dir| dir.join(".git").exists()),
            "fixture repository: {}",
            agent.workspace.root().display()
        );
        assert!(agent.offered().iter().any(|name| name == "git"));
    });
    with_agent("[harness]\ntools=['read','bash']\n", |agent| {
        assert!(
            agent
                .workspace
                .root()
                .ancestors()
                .any(|dir| dir.join(".git").exists())
        );
        assert!(!agent.offered().iter().any(|name| name == "git"));
    });
}

#[test]
fn a_valid_call_to_an_unloaded_deferred_tool_runs_in_the_same_round() {
    let server = llm::fake::serve(vec![
        calls(&[("git", json!({"action":"status"}))]),
        slots(),
        response("Status read.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        assert!(
            !agent.offered().iter().any(|name| name == "git"),
            "git starts deferred"
        );
        let report = agent.turn("Show the git status", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert!(
            agent.offered().iter().any(|name| name == "git"),
            "and stays loaded"
        );
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 1);
    assert!(!results[0].contains("invalid tool call"), "{}", results[0]);
}

#[test]
fn an_invalid_call_to_an_unloaded_deferred_tool_is_refused_with_its_schema() {
    let server = llm::fake::serve(vec![
        calls(&[("git", json!({"action":"frobnicate"}))]),
        slots(),
        response("Will retry.", None),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Show the git status", &mut NullUi).unwrap();
    });
    let requests = completion_requests(&server);
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 1);
    assert!(
        results[0]
            .contains("git was loaded this round, but these arguments do not match its schema")
            && results[0].contains("this call was not executed")
            && results[0].contains("\"branch_create\""),
        "{}",
        results[0]
    );
}

const FAILING_NOTE_CHECK: &str = "[compaction]\nenabled=false\n[[verification.commands]]\nname=\"notes\"\ncommand=\"echo note check failed; exit 1\"\nfile_extensions=[\"txt\"]\ntimeout_secs=5\n";

#[test]
fn an_edit_whose_gate_check_fails_is_reminded_once_then_disclosed() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("write", json!({"path":"note.txt","content":"x\n"}))),
        ),
        slots(),
        response("The note is written.", None),
        response("The note is written; the check still fails.", None),
    ]);
    let report = with_agent(FAILING_NOTE_CHECK, |agent| {
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
    assert!(mentions(&requests[2], "they failed"));
    assert!(
        mentions(&requests[2], "note check failed"),
        "the reminder carries the gate's own check output"
    );
    assert!(!mentions(&requests[1], "they failed"));
}

#[test]
fn the_completion_gate_runs_a_passing_check_instead_of_spending_a_reminder_round() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("write", json!({"path":"note.txt","content":"x\n"}))),
        ),
        slots(),
        response("The note is written.", None),
    ]);
    let report = with_agent(NOTE_CHECK, |agent| {
        connect(agent, &server.url);
        let report = agent.turn("Write the note", &mut NullUi).unwrap();
        assert_eq!(
            agent.step_checks.evidence.latest_status("test -f note.txt"),
            Some(crate::agent::verify::verification::CheckStatus::Passed)
        );
        report
    });
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert!(!report.text.contains("Unverified"), "{}", report.text);
    assert_eq!(
        completion_requests(&server).len(),
        2,
        "no reminder round when the gate's own run passes"
    );
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
            .any(|request| mentions(request, "Run the relevant check"))
    );
}

/// A tool call written as text, which the turn rejects as unusable.
fn text_call() -> llm::fake::Script {
    response(r#"{"name":"read","arguments":{"path":"src/lib.rs"}}"#, None)
}

#[test]
fn alternating_recoveries_without_progress_stop_without_a_redirect_prompt() {
    // Each unusable reply is followed by a failing tool call. Both consume one shared
    // recovery attempt; resetting the unusable-reply cap cannot conceal the failed-tool rounds.
    let mut scripts = vec![text_call(), slots()];
    for index in 0..8 {
        scripts.push(response(
            "",
            Some(("read", json!({"path":format!("missing-{index}.txt")}))),
        ));
        if index < 7 {
            scripts.push(text_call());
        }
    }
    let server = llm::fake::serve(scripts);
    let report = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Read the file", &mut NullUi).unwrap()
    });
    assert_eq!(report.outcome, TurnOutcome::Blocked, "{}", report.text);
    assert!(
        report.text.contains("16 recovery attempts in a row")
            && report.text.contains("unusable tool call ×8")
            && report
                .text
                .contains("tool round without productive progress ×8"),
        "{}",
        report.text
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 16, "no request after the ladder stops");
    assert!(
        requests
            .iter()
            .all(|request| !mentions(request, "6 recovery attempts in a row")),
        "the progress allowance must not inject a redirect instruction"
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
fn an_auto_backgrounded_command_loads_bg_for_the_next_request() {
    let server = llm::fake::serve(vec![
        response("", Some(("bash", json!({"command":"sleep 3; echo done"})))),
        slots(),
        response("The command is running in the background.", None),
        // The job finishes during the turn; Dial asks once more to inspect its terminal result.
        response("The background command finished.", None),
    ]);
    with_agent(
        "approval='auto'\n[compaction]\nenabled=false\n[verification]\nenabled=false\n[shell]\nauto_background_secs=1",
        |agent| {
            connect(agent, &server.url);
            agent.modes.launch.unattended = true;
            assert!(agent.bg_loadable(), "{:?}", agent.offered());
            agent.turn("Start the slow command.", &mut NullUi).unwrap();
            assert!(agent.offered().iter().any(|name| name == "bg"));
        },
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3);
    let offers_bg = |request: &Value| {
        request["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "bg"))
    };
    assert!(!offers_bg(&requests[0]));
    assert!(offers_bg(&requests[1]));
    let result = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .to_string();
    assert!(
        result.contains("background job")
            && result.contains("`bg wait` returns its output")
            && !result.contains("tool_load"),
        "{result}"
    );
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

#[test]
fn a_known_tool_typed_into_bash_gets_direct_coaching_and_the_unchanged_retry_stays_withheld() {
    // `todo` is a Dial tool name that is not a shell program, so the real shell reports it missing.
    let typed = json!({"command":"todo list"});
    let server = llm::fake::serve(vec![
        calls(&[("bash", typed.clone())]),
        slots(),
        calls(&[("bash", typed)]),
        response("Reported the limit.", None),
    ]);
    let report = with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.turn("Show the todo list", &mut NullUi).unwrap()
    });
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "todo"),
        "direct coaching requires the tool's supplied schema"
    );
    let tool_results = |request: &Value| -> Vec<String> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let first = tool_results(&requests[1]);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(
        first[0].contains("tool_not_shell")
            && first[0].contains("Call it directly with its supplied schema")
            && !first[0].contains("tool_load"),
        "{}",
        first[0]
    );
    let second = tool_results(&requests[2]);
    assert_eq!(second.len(), 2, "{second:?}");
    assert_eq!(
        second[0], first[0],
        "the first result is replayed unchanged"
    );
    assert!(
        second[1].contains("retry barrier") && second[1].contains("not executed again"),
        "the identical retry must stay withheld: {}",
        second[1]
    );
    let original: String = first[0]
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap()
        .chars()
        .take(60)
        .collect();
    assert!(
        second[1].contains("Its original error was:") && second[1].contains(&original),
        "the barrier quotes the original error: {}",
        second[1]
    );
}

#[test]
fn an_empty_tool_search_returns_catalog_without_enabling_a_capability() {
    let server = llm::fake::serve(vec![
        response("", Some(("tool_search", json!({"query":"zzqxjvkmnop"})))),
        slots(),
        response("No such capability is available.", None),
    ]);
    // Pin a small offer: context-budget trimming is independent of discovery and may remove
    // incidental tools between requests. Here the callable surface must remain exactly stable.
    let report = with_agent(
        "[compaction]\nenabled=false\n[assistance]\ncompact_tools=true\n[harness]\ntools=['read','tool_search','tool_load']\n",
        |agent| {
            connect(agent, &server.url);
            agent.turn("Find a zzqx capability", &mut NullUi).unwrap()
        },
    );
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    let offered = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        offered(&requests[0])
            .iter()
            .any(|name| name == "tool_search"),
        "tool_search must be offered, so the empty result comes from a real dispatch"
    );
    let result = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let page: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(page["search_matches"], json!([]));
    assert_eq!(page["listing"], true);
    assert!(page["tools"].as_array().unwrap().len() <= 16);
    assert_eq!(
        offered(&requests[0]),
        offered(&requests[1]),
        "an empty search must not change the next request's callable tools"
    );
}

#[test]
fn discovery_recovery_uses_the_next_requests_policy_and_is_delivered_once() {
    for one_call in [false, true] {
        let server = llm::fake::serve(vec![
            response(
                "",
                Some(("grep", json!({"pattern":"name:","path":"registry.rs"}))),
            ),
            slots(),
            response(
                "",
                Some((
                    "grep",
                    json!({"pattern":"CommandSpec", "path":"registry.rs"}),
                )),
            ),
            response("", Some(("read", json!({"path":"registry.rs"})))),
            response("There are two tuple commands: help and exit.", None),
        ]);
        with_agent(
            "[compaction]\nenabled=false\n[reliability]\nmeasured_interfaces=true\n",
            |agent| {
                connect(agent, &server.url);
                std::fs::write(agent.workspace.root().join("registry.rs"), "const COMMANDS: &[(&str, &str)] = &[(\"help\", \"Help\"), (\"exit\", \"Exit\")];\n").unwrap();
                if one_call {
                    std::fs::create_dir_all(&agent.config.state_dir).unwrap();
                    let record = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":crate::reliability::binding(agent.config),"batch_passed":false,"passed":1,"total":2});
                    std::fs::write(
                        agent.config.state_dir.join("reliability.json"),
                        serde_json::to_vec(&record).unwrap(),
                    )
                    .unwrap();
                }
                let report = agent
                    .turn(
                        "Inspect registry.rs and report the command names.",
                        &mut NullUi,
                    )
                    .unwrap();
                assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
                let control = agent.session.control_state();
                let policy = &control["reliability"]["call_policy"];
                assert_eq!(policy["mode"], if one_call { "one_call" } else { "batch" });
                assert_eq!(
                    policy["reasons"],
                    if one_call {
                        json!(["saved_reliability_record"])
                    } else {
                        json!([])
                    }
                );
                assert_eq!(
                    control["adaptive_policy"]["selection"]["call_policy"],
                    *policy
                );
                assert!(
                    control["adaptive_history"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|entry| entry["selection"]["call_policy"] == *policy)
                );
            },
        );
        let requests = completion_requests(&server);
        assert_eq!(
            requests.len(),
            4,
            "the harness should not add inference rounds; raw requests: {:?}",
            server
                .requests()
                .iter()
                .map(|request| String::from_utf8_lossy(request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned())
                .collect::<Vec<_>>()
        );
        assert!(!mentions(&requests[0], "[Discovery recovery]"));
        assert!(!mentions(&requests[1], "[Discovery recovery]"));
        assert!(mentions(&requests[2], "[Discovery recovery]"));
        assert_eq!(mentions(&requests[2], "Gather unrelated docs"), !one_call);
        assert_eq!(
            mentions(&requests[2], "inspect that declaration next"),
            one_call
        );
        assert!(
            !mentions(&requests[3], "[Discovery recovery]"),
            "a delivered correction must retire"
        );
    }
}

#[test]
fn turn_settlement_keeps_post_edit_evidence_current_without_a_check_request() {
    let command = "grep -q after candidate.txt";
    for tool in ["edit", "write"] {
        let mutations = if tool == "edit" {
            vec![
                ("read", json!({"path":"candidate.txt"})),
                (
                    "edit",
                    json!({"path":"candidate.txt","old_string":"before","new_string":"after","check":true}),
                ),
            ]
        } else {
            vec![(
                "write",
                json!({"path":"candidate.txt","content":"after\n","check":true}),
            )]
        };
        let server = llm::fake::serve(vec![
            calls(&mutations),
            slots(),
            response("The change and its focused check completed.", None),
        ]);
        let overlay = "approval='auto'\n[compaction]\nenabled=false\n[harness]\nedit_mode='replace'\n[edit]\nstreaming_abort=false\n[verification]\nautomatic_checks=false\n[[verification.commands]]\nname='candidate'\ncommand='grep -q after candidate.txt'\ntimeout_secs=5\n";
        with_agent(overlay, |agent| {
            if tool == "edit" {
                std::fs::write(agent.workspace.root().join("candidate.txt"), "before\n").unwrap();
            }
            connect(agent, &server.url);
            let report = agent
                .turn("Apply and check the candidate", &mut NullUi)
                .unwrap();
            assert_eq!(
                report.outcome,
                TurnOutcome::Completed,
                "{tool}: {}",
                report.text
            );
            assert_eq!(
                agent.step_checks.mutations, 1,
                "settlement must not double-count {tool}"
            );
            assert_eq!(
                agent
                    .step_checks
                    .evidence
                    .current_status(command, agent.step_checks.revision()),
                "passed"
            );
            let messages = agent.session.messages();
            let results: Vec<_> = messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            assert_eq!(
                results.len(),
                mutations.len(),
                "no separate check call: {results:?}"
            );
            assert!(
                results.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .contains("passed: candidate"),
                "{results:?}"
            );
        });
        assert_eq!(
            completion_requests(&server).len(),
            2,
            "one {tool} request and one final response; no separate check round"
        );
    }
}

#[test]
fn failed_post_edit_check_blocks_same_batch_completion_without_refusing_the_edit() {
    for tool in ["edit", "write"] {
        let mut mutations = if tool == "edit" {
            vec![
                ("read", json!({"path":"candidate.txt"})),
                (
                    "edit",
                    json!({"path":"candidate.txt","old_string":"before","new_string":"after","check":true}),
                ),
            ]
        } else {
            vec![(
                "write",
                json!({"path":"candidate.txt","content":"after\n","check":true}),
            )]
        };
        let mutation_index = mutations.len() - 1;
        mutations.push(("finish", json!({"outcome":"completed","summary":"Done"})));
        let server = llm::fake::serve(vec![calls(&mutations), slots()]);
        let overlay = "approval='auto'\n[compaction]\nenabled=false\n[harness]\nedit_mode='replace'\n[edit]\nstreaming_abort=false\n[verification]\nautomatic_checks=false\n[[verification.commands]]\nname='candidate'\ncommand='echo AssertionError: candidate failed; exit 1'\ntimeout_secs=5\n";
        with_agent(overlay, |agent| {
            let path = agent.workspace.root().join("candidate.txt");
            if tool == "edit" {
                std::fs::write(&path, "before\n").unwrap();
            }
            connect(agent, &server.url);
            let report = agent
                .turn("Apply and check the candidate", &mut NullUi)
                .unwrap();
            assert_ne!(
                report.outcome,
                TurnOutcome::Completed,
                "{tool}: {}",
                report.text
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
            let messages = agent.session.messages();
            let results: Vec<_> = messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            assert_eq!(
                results[mutation_index]["is_error"], false,
                "the {tool} was applied: {results:?}"
            );
            assert!(
                results[mutation_index]["content"]
                    .as_str()
                    .unwrap()
                    .contains("code_failed: candidate")
            );
            assert!(
                results[mutation_index + 1]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Completion cannot follow a failed or refused call"),
                "{results:?}"
            );
            assert!(agent.step_checks.failed);
            assert_eq!(agent.step_checks.mutations, 1);
        });
        assert_eq!(
            completion_requests(&server).len(),
            1,
            "same-batch finish is downgraded without another model request"
        );
    }
}

#[test]
fn covered_native_reads_reuse_only_visible_rows_in_serial_and_parallel_turns() {
    fn reads(id: &str, paths: &[&str]) -> llm::fake::Script {
        let calls: Vec<Value> = paths.iter().enumerate().map(|(index, path)| {
            json!({"index":index,"id":format!("{id}-{index}"),"type":"function","function":{"name":"read","arguments":json!({"path":path}).to_string()}})
        }).collect();
        let event = json!({"choices":[{"delta":{"content":"","tool_calls":calls},"finish_reason":"tool_calls"}]});
        llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
    }
    for parallel in [false, true] {
        for clipped in [false, true] {
            let mut scripts = vec![reads("seed", &["source.txt:1-20"]), slots()];
            if parallel {
                scripts.push(reads("follow", &["source.txt:2-5", "source.txt:10-30"]));
            } else {
                scripts.push(reads("follow", &["source.txt:2-5"]));
                scripts.push(reads("overlap", &["source.txt:10-30"]));
            }
            scripts.push(response("Inspection complete.", None));
            let server = llm::fake::serve(scripts);
            // Read previews retain the per-model limit rather than tool_preview_bytes.
            let overlay = "context_window=32768\n[compaction]\nenabled=false\n[verification]\nenabled=false\n[harness]\nedit_mode='hashline'\n[experimental]\nstreaming_dispatch=false\n";
            with_agent(overlay, |agent| {
                let source = (1..=30)
                    .map(|n| {
                        format!(
                            "source {n} {}\n",
                            "x".repeat(if clipped { 900 } else { 80 })
                        )
                    })
                    .collect::<String>();
                std::fs::write(agent.workspace.root().join("source.txt"), source).unwrap();
                connect(agent, &server.url);
                let report = agent
                    .turn(
                        "Inspect these source ranges without changing the file",
                        &mut NullUi,
                    )
                    .unwrap();
                assert_eq!(
                    report.outcome,
                    TurnOutcome::Completed,
                    "parallel={parallel} clipped={clipped}: {}",
                    report.text
                );
                let messages = agent.session.messages();
                let result = |id: &str| {
                    messages
                        .iter()
                        .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
                        .unwrap()
                };
                let seed = result("seed-0");
                let covered = result("follow-0")["content"].as_str().unwrap();
                let overlap = result(if parallel { "follow-1" } else { "overlap-0" })["content"]
                    .as_str()
                    .unwrap();
                assert!(
                    covered.starts_with("[Unchanged read] Source SHA-256 revalidated"),
                    "{covered}"
                );
                // Native output budgeting can omit whole rows without clipping any displayed row.
                let first_missing = if clipped { 17 } else { 21 };
                if clipped {
                    assert!(
                        seed["content"]
                            .as_str()
                            .unwrap()
                            .contains("line 17 does not fit")
                    );
                }
                assert!(
                    overlap.contains(&format!("{first_missing}:source {first_missing}"))
                        && !overlap.contains("10:source 10"),
                    "{overlap}"
                );
                assert!(overlap.contains("[Read reuse]"), "{overlap}");
            });
        }
    }
}

#[test]
fn presentation_reduced_source_results_cannot_authorize_read_reuse() {
    with_agent("", |agent| {
        std::fs::write(
            agent.workspace.root().join("source.txt"),
            "source\n".repeat(30),
        )
        .unwrap();
        let call = ToolCall {
            id: "source-result".into(),
            name: "read".into(),
            arguments: json!({"path":"source.txt:1-30"}),
            malformed_arguments: false,
        };
        let output = tools::execute(&agent.workspace, "read", &call.arguments, 10);
        assert!(!output.is_error, "{}", output.text);
        let mut returned = crate::agent::workflow::investigation::ReturnedReads::default();
        assert!(
            returned
                .pointer(&call, &output.text, false, |_| true)
                .is_none()
        );
        let mut message = crate::tools::surface::outcome::message(&call.id, &output);
        message["content"] = json!("[source.txt#AAAA]\n1:source\n[output omitted]");
        message["dial_execution"]["output"] = json!({"presentation":"partial"});
        agent.push(message).unwrap();
        assert!(!agent.holds_full_result(&call.id));
        let mut invoked = 0;
        returned.reusable(|id| agent.holds_full_result(id)).execute(
            &agent.workspace,
            &call.arguments,
            |args| {
                invoked += 1;
                assert_eq!(args, &call.arguments);
                tools::execute(&agent.workspace, "read", args, 10)
            },
        );
        assert_eq!(
            invoked, 1,
            "presentation clipping must allow a fresh observation"
        );
    });
}
