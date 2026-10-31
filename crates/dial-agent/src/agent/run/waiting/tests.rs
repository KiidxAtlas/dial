use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::tools::exec::background::{AdoptedJob, CellJob};
use std::sync::mpsc;

struct ReleaseOnWait {
    release: Option<mpsc::Sender<()>>,
    waits: usize,
    input: bool,
}
impl Ui for ReleaseOnWait {
    fn notice(&mut self, text: &str) {
        if text.starts_with("Waiting for running work without model polling:") {
            self.waits += 1;
            // Also exercise a wake before the controller enters its first kernel wait.
            crate::platform::wake::signal();
            if let Some(release) = self.release.take() {
                release.send(()).unwrap();
            }
        }
    }
    fn pending_input(&mut self) -> bool {
        self.input
    }
}

fn pending_cell(agent: &Agent<'_>, result: &str) -> (String, mpsc::Sender<()>) {
    let (send, receive) = mpsc::channel();
    let result = result.to_owned();
    let name = agent
        .ext
        .get("bg")
        .unwrap()
        .adopt_job(AdoptedJob::Cell(CellJob {
            command: "deterministic test work".into(),
            network: false,
            work: Box::new(move |_| {
                receive
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test must release the cell");
                result
            }),
        }))
        .unwrap();
    (name, send)
}

#[test]
fn no_owned_work_does_not_suspend_or_request_continuation() {
    with_agent("", |agent| {
        let mut ui = ReleaseOnWait {
            release: None,
            waits: 0,
            input: false,
        };
        assert!(!agent.wait_for_background_event(&mut ui).unwrap());
        assert_eq!(ui.waits, 0);
    });
}

#[test]
fn owned_completion_wakes_the_wait_and_is_queued_exactly_once() {
    with_agent("", |agent| {
        let (name, release) =
            pending_cell(agent, "distinct failing diagnostic, not a success claim");
        let mut ui = ReleaseOnWait {
            release: Some(release),
            waits: 0,
            input: false,
        };
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert_eq!(ui.waits, 1);
        assert!(agent.awaited_background_jobs().is_empty());
        let notices = std::mem::take(&mut agent.background_notices);
        assert_eq!(
            notices
                .iter()
                .filter(|note| note.contains(&format!("job {name} finished")))
                .count(),
            1
        );
        let mut backend = agent.backend.take().unwrap();
        let output = agent.dispatch(
            &mut backend,
            &ToolCall {
                id: "inspect-result".into(),
                name: "bg".into(),
                arguments: json!({"action":"output","name":name}),
                malformed_arguments: false,
            },
            &mut ui,
        );
        agent.backend = Some(backend);
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("distinct failing diagnostic"));
        agent.pump_subagents(&mut ui);
        assert!(
            agent.background_notices.is_empty(),
            "completion must not be replayed"
        );
        assert!(!agent.wait_for_background_event(&mut ui).unwrap());
    });
}

#[test]
fn queued_input_ends_the_wait_without_consuming_input_or_killing_the_job() {
    with_agent("", |agent| {
        let (_name, release) = pending_cell(agent, "still managed");
        let mut ui = ReleaseOnWait {
            release: None,
            waits: 0,
            input: true,
        };
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert!(ui.input, "the normal step boundary owns input delivery");
        assert_eq!(agent.awaited_background_jobs().len(), 1);
        ui.input = false;
        ui.release = Some(release);
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert!(agent.awaited_background_jobs().is_empty());
    });
}

#[test]
fn peer_mail_ends_the_wait_and_reaches_the_next_request_while_the_work_keeps_running() {
    with_agent("", |agent| {
        let (_name, release) = pending_cell(agent, "still managed");
        let own_id = agent.peer_board().unwrap().id;
        let peer = crate::storage::coordination::Participant::join(
            agent.layout().unwrap(),
            "other-session",
        )
        .unwrap();
        peer.send(&own_id, "the interface changed").unwrap();
        let mut ui = ReleaseOnWait {
            release: None,
            waits: 0,
            input: false,
        };
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert_eq!(
            agent.awaited_background_jobs().len(),
            1,
            "the job is untouched"
        );
        // The wait only noticed the letter: the request boundary is what takes and shows it.
        let notes = agent.poll_coordination(&mut ui);
        assert!(
            notes[0]["content"]
                .as_str()
                .unwrap()
                .contains("the interface changed")
        );
        // With the mail delivered the wait goes back to waiting for the work itself.
        ui.release = Some(release);
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert!(agent.awaited_background_jobs().is_empty());
    });
}

#[test]
fn deadline_stops_suspension_without_marking_running_work_complete() {
    with_agent("", |agent| {
        let (_name, release) = pending_cell(agent, "not completed at the deadline");
        agent.modes.launch.deadline = Some(Instant::now());
        let mut ui = ReleaseOnWait {
            release: None,
            waits: 0,
            input: false,
        };
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert!(agent.stop_boundary().is_some());
        assert_eq!(agent.awaited_background_jobs().len(), 1);
        agent.modes.launch.deadline = None;
        ui.release = Some(release);
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
    });
}

#[test]
fn completion_tool_defers_acceptance_until_the_new_result_is_incorporated() {
    with_agent("", |agent| {
        let (_name, release) = pending_cell(agent, "worker evidence");
        let mut ui = ReleaseOnWait {
            release: Some(release),
            waits: 0,
            input: false,
        };
        let mut backend = agent.backend.take().unwrap();
        let output = agent.dispatch(
            &mut backend,
            &ToolCall {
                id: "finish-probe".into(),
                name: "finish".into(),
                arguments: json!({"outcome":"completed","summary":"Completed the requested work."}),
                malformed_arguments: false,
            },
            &mut ui,
        );
        agent.backend = Some(backend);
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(
            output.execution.effects,
            crate::tools::surface::outcome::Effects::None
        );
        assert!(
            !agent.step_checks.finish_accepted,
            "a wait is not completion evidence"
        );
        assert!(!agent.background_notices.is_empty());
        assert_eq!(ui.waits, 1);
    });
}

#[test]
fn tool_free_yield_waits_for_an_event_before_the_next_model_request() {
    use crate::llm::fake::{self, Script};
    let sse = |text: &str| {
        let event = json!({"choices":[{"delta":{"content":text}}]});
        let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        Script::Sse(format!(
            "data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n"
        ))
    };
    let server = fake::serve(vec![
        sse("Waiting for the running work."),
        fake::json(200, &json!({"total_slots":1})),
        sse("The completed work reported the expected diagnostic."),
    ]);
    with_agent("[harness]\nmax_steps=3", |agent| {
        agent.backend = Some(Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        ));
        let (_name, release) = pending_cell(agent, "expected diagnostic");
        let mut ui = ReleaseOnWait {
            release: Some(release),
            waits: 0,
            input: false,
        };
        let report = agent
            .turn(
                "Inspect the result of the running work and report it. Do not edit files.",
                &mut ui,
            )
            .unwrap_or_else(|error| {
                let requests: Vec<_> = server
                    .requests()
                    .iter()
                    .map(|request| {
                        String::from_utf8_lossy(request)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_owned()
                    })
                    .collect();
                panic!("{error:#}; requests: {requests:?}");
            });
        assert_eq!(report.outcome, TurnOutcome::Completed);
        assert_eq!(
            ui.waits, 1,
            "the first yield must suspend rather than request polling"
        );
        let requests: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|request| request.starts_with(b"POST /v1/chat/completions "))
            .collect();
        assert_eq!(requests.len(), 2);
        let second = fake::body(&requests[1]);
        assert!(
            second["messages"].to_string().contains("finished]"),
            "{second}"
        );
    });
}

#[test]
fn interrupted_wait_returns_without_reporting_success() {
    with_agent("", |agent| {
        let (_name, release) = pending_cell(agent, "result after interruption");
        agent.interrupt.store(true, Ordering::Release);
        let mut ui = ReleaseOnWait {
            release: None,
            waits: 0,
            input: false,
        };
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
        assert!(agent.interrupt.load(Ordering::Acquire));
        assert_eq!(agent.awaited_background_jobs().len(), 1);
        agent.interrupt.store(false, Ordering::Release);
        ui.release = Some(release);
        assert!(agent.wait_for_background_event(&mut ui).unwrap());
    });
}

#[test]
fn final_request_reports_pending_work_without_suspending_for_its_completion() {
    let event = json!({"choices":[{"delta":{"content":"The requested job is not finished. Its result is unverified."},"finish_reason":"stop"}]});
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Sse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    ))]);
    with_agent(
        "[harness]\nmax_steps=1\n[compaction]\nenabled=false",
        |agent| {
            agent.version.harness.max_steps = 1;
            agent.modes.launch.deadline = Some(Instant::now() + Duration::from_secs(3));
            agent.backend = Some(Backend::new(
                ProviderKind::Endpoint {
                    base_url: format!("{}/v1", server.url),
                    model: "test".into(),
                    api_key: None,
                },
                PathBuf::new(),
                5,
            ));
            let (_name, release) = pending_cell(agent, "eventual result");
            let mut ui = ReleaseOnWait {
                release: None,
                waits: 0,
                input: false,
            };
            let report = agent
                .turn(
                    "Report what remains pending without editing files.",
                    &mut ui,
                )
                .unwrap();
            assert_eq!(
                ui.waits, 0,
                "the final answer must not be held for running work"
            );
            assert_eq!(report.outcome, TurnOutcome::Incomplete);
            assert_eq!(agent.awaited_background_jobs().len(), 1);
            release.send(()).unwrap();
            agent.background_notices.clear();
            assert!(agent.wait_for_background_event(&mut ui).unwrap());
        },
    );
    assert_eq!(server.requests().len(), 1);
}
