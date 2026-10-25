use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

#[derive(Default)]
struct Input {
    pending: Vec<String>,
    accepted: Vec<String>,
    notices: Vec<String>,
    cancel: Option<Arc<AtomicBool>>,
    during_reply: Option<String>,
    independent: Vec<String>,
    deferred: Vec<String>,
    applied: Vec<String>,
}
impl Ui for Input {
    fn request_phase(&mut self, phase: crate::agent::RequestPhase) {
        if phase == crate::agent::RequestPhase::Generating
            && let Some(message) = self.during_reply.take()
        {
            self.pending.push(message);
        }
    }
    fn take_pending_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }
    fn restore_pending_messages(&mut self, messages: Vec<String>) {
        self.pending = messages;
    }
    fn pending_message_accepted(&mut self, text: &str) {
        self.accepted.push(text.into());
    }
    fn take_independent_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.independent)
    }
    fn defer_pending_messages(&mut self, messages: Vec<String>) {
        self.deferred.extend(messages);
    }
    fn steering_applied(&mut self, text: &str) {
        self.applied.push(text.into());
    }
    fn notice(&mut self, text: &str) {
        self.notices.push(text.into());
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
    }
    fn approve(&mut self, _: &str, _: &str) -> crate::agent::Decision {
        crate::agent::Decision::Allow
    }
}
fn reply(text: &str) -> llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}
fn backend(url: &str) -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
}
fn active(agent: &mut Agent<'_>) {
    let mut state = agent.session.control_state();
    state["current_instruction"] = json!("Fix the parser");
    agent.session.set_control_state(state).unwrap();
}

#[test]
fn routing_decisions_reject_unknown_actions_and_model_supplied_assignments() {
    assert_eq!(
        decision(r#"{"route":"steer","reason":"Changes the approach"}"#)
            .unwrap()
            .route,
        Route::Steer
    );
    assert_eq!(
        decision(r#"{"route":"split","reason":"Independent request"}"#)
            .unwrap()
            .route,
        Route::Split
    );
    for invalid in [
        r#"{"route":"queue","reason":"unknown"}"#,
        r#"{"route":"split","reason":""}"#,
        r#"{"route":"split","reason":"x","task":"replace the user assignment"}"#,
        "```json\n{\"route\":\"split\",\"reason\":\"x\"}\n```",
    ] {
        assert!(decision(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn split_uses_exact_user_request_and_isolated_worker_not_model_written_work() {
    let text = "Also check the docs @guide.md\nKeep all headings.";
    let args = split_args(text, "Fix the parser");
    assert_eq!(args["task"], text);
    assert_eq!(args["agent"], "worker");
    assert_eq!(args["action"], "start");
    assert_eq!(args["isolation"], "copy");
    let chat = request("Fix the parser", &[], text, true);
    assert!(chat.tools.is_empty());
    assert_eq!(chat.max_tokens, 256);
    let input: Value = serde_json::from_str(chat.messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["incoming_message"], text);
    assert_eq!(input["active_instruction"], "Fix the parser");
}

#[test]
fn model_selected_steering_is_committed_once_and_changes_the_current_instruction() {
    let server = llm::fake::serve(vec![reply(
        r#"{"route":"steer","reason":"Change to the current approach"}"#,
    )]);
    with_agent("", |agent| {
        active(agent);
        let text = "Use a state machine instead";
        let mut ui = Input {
            pending: vec![text.into()],
            ..Default::default()
        };
        assert!(
            agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert_eq!(agent.session.control_state()["current_instruction"], text);
        assert_eq!(
            agent
                .session
                .messages()
                .iter()
                .filter(|m| m["content"] == text)
                .count(),
            1
        );
        assert_eq!(ui.accepted, [text]);
        assert!(
            !agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert!(agent.live_tasks.running_ids().is_empty());
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn malformed_routing_response_falls_back_without_losing_or_rewriting_input() {
    let server = llm::fake::serve(vec![reply("I cannot decide.")]);
    with_agent("", |agent| {
        active(agent);
        let text = "Could you change that?";
        let mut ui = Input {
            pending: vec![text.into()],
            ..Default::default()
        };
        assert!(
            agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert_eq!(agent.session.control_state()["current_instruction"], text);
        assert!(
            ui.notices
                .iter()
                .any(|s| s.contains("keeping it as steering"))
        );
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn independent_request_starts_real_worker_without_steering_parent() {
    let server = llm::fake::serve(vec![
        reply(r#"{"route":"split","reason":"Independent question"}"#),
        reply("The answer is 4."),
    ]);
    with_agent(
        &format!(
            "[endpoint]\nbase_url='{}/v1'\n[assistance.local]\nenabled=false",
            server.url
        ),
        |agent| {
            active(agent);
            let mut ui = Input {
                pending: vec!["What is 2 + 2?".into()],
                ..Default::default()
            };
            assert!(
                !agent
                    .route_pending_messages(&mut backend(&server.url), &mut ui)
                    .unwrap(),
                "{:?}",
                ui.notices
            );
            assert_eq!(
                agent.session.control_state()["current_instruction"],
                "Fix the parser"
            );
            assert!(
                !agent
                    .session
                    .messages()
                    .iter()
                    .any(|m| m["content"] == "What is 2 + 2?")
            );
            assert_eq!(agent.live_tasks.running_ids().len(), 1);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !agent.live_tasks.running_ids().is_empty() && Instant::now() < deadline {
                agent.pump_subagents(&mut ui);
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                agent.live_tasks.running_ids().is_empty(),
                "worker did not settle"
            );
            assert!(
                agent
                    .live_tasks
                    .status(None)
                    .unwrap()
                    .to_string()
                    .contains("The answer is 4.")
            );
        },
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        llm::fake::body(&requests[1])["messages"]
            .to_string()
            .contains("What is 2 + 2?")
    );
}

#[test]
fn disabled_spawning_routes_exact_input_without_a_classification_request() {
    let server = llm::fake::serve(vec![]);
    with_agent("[task]\nmax_recursion_depth=0", |agent| {
        active(agent);
        let mut ui = Input {
            pending: vec!["An independent question".into()],
            ..Default::default()
        };
        assert!(
            agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert!(agent.live_tasks.running_ids().is_empty());
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "An independent question"
        );
        assert_eq!(ui.accepted, ["An independent question"]);
    });
    assert!(server.requests().is_empty());
}

#[test]
fn cancellation_restores_all_unhandled_messages_in_order_without_accepting_them() {
    with_agent("", |agent| {
        active(agent);
        let mut ui = Input {
            pending: vec!["first".into(), "second".into()],
            cancel: Some(agent.interrupt.clone()),
            ..Default::default()
        };
        // The decision notice triggers cancellation before the request. The transport observes it;
        // no worker may start and neither pending message may be committed as steering.
        assert!(
            !agent
                .route_pending_messages(&mut backend("http://127.0.0.1:9"), &mut ui)
                .unwrap()
        );
        assert_eq!(ui.pending, ["first", "second"]);
        assert!(ui.accepted.is_empty());
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "Fix the parser"
        );
    });
}

#[test]
fn oversized_message_is_preserved_as_steering_without_a_classifier_request() {
    with_agent("", |agent| {
        active(agent);
        let text = "a".repeat(16_001);
        let mut ui = Input {
            pending: vec![text.clone()],
            ..Default::default()
        };
        assert!(
            agent
                .route_pending_messages(&mut backend("http://127.0.0.1:9"), &mut ui)
                .unwrap()
        );
        assert!(
            agent
                .session
                .messages()
                .iter()
                .any(|m| m["content"] == text)
        );
        assert_eq!(ui.accepted, [text]);
    });
}

#[test]
fn denied_worker_admission_falls_back_to_steering_without_starting_work() {
    let server = llm::fake::serve(vec![reply(
        r#"{"route":"split","reason":"Independent question"}"#,
    )]);
    with_agent("", |agent| {
        active(agent);
        agent
            .always
            .push(crate::agent::safety::approval::PermissionRule {
                effect: crate::agent::safety::approval::Effect::Deny,
                tool: "task".into(),
                command_prefix: vec![],
                paths: vec![],
            });
        let mut ui = Input {
            pending: vec!["Independent question".into()],
            ..Default::default()
        };
        assert!(
            agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert!(agent.live_tasks.running_ids().is_empty());
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "Independent question"
        );
        assert_eq!(ui.accepted, ["Independent question"]);
        assert!(
            ui.notices
                .iter()
                .any(|text| text.contains("Could not start an independent worker"))
        );
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn automatic_message_received_during_generation_replans_before_accepting_the_old_answer() {
    let server = llm::fake::serve(vec![
        reply("The answer is 4."),
        reply(r#"{"route":"steer","reason":"Replaces the active question"}"#),
        reply("The answer is 6."),
    ]);
    with_agent("", |agent| {
        agent.backend = Some(backend(&server.url));
        let mut ui = Input {
            during_reply: Some("Actually, what is 3 + 3?".into()),
            ..Default::default()
        };
        let report = agent.turn("What is 2 + 2?", &mut ui).unwrap();
        assert_eq!(report.text, "The answer is 6.");
        assert_eq!(ui.accepted, ["Actually, what is 3 + 3?"]);
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "Actually, what is 3 + 3?"
        );
        assert!(
            !agent
                .session
                .messages()
                .iter()
                .any(|message| message["role"] == "assistant"
                    && message["content"] == "The answer is 4.")
        );
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        llm::fake::body(&requests[2])["messages"]
            .to_string()
            .contains("Actually, what is 3 + 3?")
    );
}

#[test]
fn occupied_worker_capacity_routes_next_input_without_classification() {
    for overlay in ["subagent_max_live_jobs=1", "[task]\nmax_concurrency=1"] {
        let server = llm::fake::serve(vec![
            reply(r#"{"route":"split","reason":"Independent question"}"#),
            reply("The answer is 4."),
        ]);
        with_agent(
            &format!(
                "{overlay}\n[endpoint]\nbase_url='{}/v1'\n[assistance.local]\nenabled=false",
                server.url
            ),
            |agent| {
                active(agent);
                let mut ui = Input {
                    pending: vec![
                        "What is 2 + 2?".into(),
                        "Another independent question".into(),
                    ],
                    ..Default::default()
                };
                assert!(
                    agent
                        .route_pending_messages(&mut backend(&server.url), &mut ui)
                        .unwrap()
                );
                assert_eq!(
                    agent.session.control_state()["current_instruction"],
                    "Another independent question"
                );
                assert_eq!(
                    ui.accepted,
                    ["What is 2 + 2?", "Another independent question"]
                );
                // Settled but unjoined workers retain their admission slot too, so the
                // second message deterministically steers before pumping the first worker.
                assert_eq!(agent.live_tasks.running_ids().len(), 1);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !agent.live_tasks.running_ids().is_empty() && Instant::now() < deadline {
                    agent.pump_subagents(&mut ui);
                    thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    agent.live_tasks.running_ids().is_empty(),
                    "worker did not settle"
                );
            },
        );
        // One classifier and one child request, never a classifier for the second message.
        assert_eq!(server.requests().len(), 2);
    }
}

#[test]
fn explicit_independent_message_never_steers_or_calls_a_classifier() {
    let server = llm::fake::serve(vec![]);
    with_agent("[task]\nmax_recursion_depth=0", |agent| {
        active(agent);
        let mut ui = Input {
            independent: vec!["Write the changelog".into()],
            ..Default::default()
        };
        // No worker can start in this fixture: the message is handed back as a follow-up and the
        // active instruction is untouched.
        assert!(
            !agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert_eq!(ui.deferred, ["Write the changelog"]);
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "Fix the parser"
        );
        assert!(ui.applied.is_empty() && ui.accepted.is_empty());
    });
    assert!(server.requests().is_empty());
}

#[test]
fn committed_steering_reports_applied_only_after_journaling() {
    with_agent("", |agent| {
        active(agent);
        let mut ui = Input::default();
        assert!(
            agent
                .accept_steering(vec!["Use v2".into()], &mut ui)
                .unwrap()
        );
        assert_eq!(ui.applied, ["Use v2"]);
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "Use v2"
        );
    });
}

#[test]
fn settled_startup_failure_preserves_input_using_each_routes_failure_policy() {
    for explicit in [false, true] {
        let scripts = if explicit {
            vec![]
        } else {
            vec![reply(
                r#"{"route":"split","reason":"Independent question"}"#,
            )]
        };
        let server = llm::fake::serve(scripts);
        with_agent("[task]\npersist=true", |agent| {
            active(agent);
            // Make transcript creation fail after admission, returning Ok([{status: failed}])
            // rather than an Err from the admission gate. No worker thread is launched.
            let dir = crate::agent::workers::child_transcript::transcript_dir(
                agent.layout.as_ref().unwrap(),
                agent.session.id(),
            );
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            std::fs::write(&dir, "not a directory").unwrap();
            let text = "Write the changelog";
            let mut ui = Input::default();
            if explicit {
                ui.independent.push(text.into());
            } else {
                ui.pending.push(text.into());
            }
            assert_eq!(
                agent
                    .route_pending_messages(&mut backend(&server.url), &mut ui)
                    .unwrap(),
                !explicit
            );
            assert!(agent.live_tasks.running_ids().is_empty());
            assert!(
                ui.notices
                    .iter()
                    .any(|notice| notice.contains("startup did not succeed")),
                "{:?}",
                ui.notices
            );
            assert!(
                ui.notices
                    .iter()
                    .any(|notice| notice.contains("Could not start an independent worker"))
            );
            assert!(
                !ui.notices
                    .iter()
                    .any(|notice| notice.starts_with("Started an independent worker"))
            );
            if explicit {
                assert_eq!(ui.deferred, [text]);
                assert!(ui.accepted.is_empty() && ui.applied.is_empty());
                assert_eq!(
                    agent.session.control_state()["current_instruction"],
                    "Fix the parser"
                );
            } else {
                assert!(ui.deferred.is_empty());
                assert_eq!(ui.accepted, [text]);
                assert_eq!(ui.applied, [text]);
                assert_eq!(agent.session.control_state()["current_instruction"], text);
            }
        });
        assert_eq!(server.requests().len(), usize::from(!explicit));
    }
}

#[test]
fn startup_validation_accepts_only_confirmed_running_jobs() {
    assert!(started_worker(json!([{"id":"task-1","status":"running"}])).is_ok());
    for result in [
        json!([]),
        json!({"status":"running"}),
        json!([{"id":"task-1","status":"failed","result":"transcript failed"}]),
        json!([{"id":"task-1","status":"running"},{"id":"task-2","status":"failed"}]),
    ] {
        assert!(started_worker(result).is_err());
    }
}

#[test]
fn routing_deadline_is_request_local_and_does_not_interrupt_the_turn() {
    let parent = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    crate::agent::model::roles::with_request_deadline(
        parent.clone(),
        Duration::from_millis(40),
        |cancel| {
            while !cancel.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!parent.load(Ordering::SeqCst));
}

#[test]
fn silent_routing_provider_falls_back_within_one_budget_and_preserves_input() {
    let server = llm::fake::serve(vec![llm::fake::Script::Stall {
        silence: Duration::from_secs(3),
    }]);
    with_agent("worker_timeout_secs=1", |agent| {
        active(agent);
        let text = "Use a state machine instead";
        let mut ui = Input {
            pending: vec![text.into()],
            ..Default::default()
        };
        let started = Instant::now();
        assert!(
            agent
                .route_pending_messages(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!agent.interrupt.load(Ordering::SeqCst));
        assert_eq!(ui.accepted, [text]);
        assert_eq!(agent.session.control_state()["current_instruction"], text);
        assert!(
            ui.notices
                .iter()
                .any(|notice| notice.contains("keeping it as steering"))
        );
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn occupied_routing_provider_falls_back_without_waiting_or_making_a_request() {
    let server = llm::fake::serve(vec![]);
    with_agent(
        &format!("[endpoint]\nbase_url='{}/v1'", server.url),
        |agent| {
            active(agent);
            let config = agent.auth_config.for_role("small");
            let _occupied =
                crate::agent::model::provider_slots::acquire(&config, &agent.interrupt).unwrap();
            let mut ui = Input {
                pending: vec!["Use v2".into()],
                ..Default::default()
            };
            let started = Instant::now();
            assert!(
                agent
                    .route_pending_messages(&mut backend(&server.url), &mut ui)
                    .unwrap()
            );
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(ui.accepted, ["Use v2"]);
            assert!(!agent.interrupt.load(Ordering::SeqCst));
        },
    );
    assert!(server.requests().is_empty());
}

#[test]
fn configured_judge_answers_routing_instead_of_the_main_provider() {
    let main = llm::fake::serve(vec![]);
    let judge = llm::fake::serve(vec![reply(
        r#"{"route":"steer","reason":"Changes approach"}"#,
    )]);
    with_agent(
        &format!(
            "[model_roles.judge]\nprovider='endpoint'\nmodel='judge-test'\nbase_url='{}/v1'",
            judge.url
        ),
        |agent| {
            active(agent);
            let mut ui = Input {
                pending: vec!["Use v2".into()],
                ..Default::default()
            };
            assert!(
                agent
                    .route_pending_messages(&mut backend(&main.url), &mut ui)
                    .unwrap()
            );
            assert_eq!(ui.accepted, ["Use v2"]);
        },
    );
    assert!(main.requests().is_empty());
    assert_eq!(judge.requests().len(), 1);
}
