//! Request controls cannot leak through a phase error into a later backend consumer.
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::agent::run::turn::completion_tests::{completion_requests, connect};

#[test]
fn preparation_failure_clears_the_pending_profile_before_the_backend_is_reused() {
    let server = llm::fake::serve(vec![llm::fake::json(
        200,
        &json!({
            "choices": [{"message": {"role": "assistant", "content": "Fresh request."}, "finish_reason": "stop"}]
        }),
    )]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &[], 5, "goal", &mut NullUi)
            .unwrap();
        let mut profile = crate::llm::profiles::ModelProfile::minimal("endpoint", "test");
        profile.max_tokens = Some(7);
        backend.next_request_profile(Some(profile));

        // Invalid tool contracts fail during preparation, before inference consumes controls.
        agent
            .turn_step(
                &mut turn,
                &mut crate::agent::run::engine::Engine::default(),
                &mut backend,
                &mut vec![Value::Null],
                &mut NullUi,
            )
            .err()
            .expect("the invalid contract must fail before any request is sent");

        // Bypass turn preparation: it would overwrite the leaked profile and hide the defect.
        backend
            .chat(
                None,
                &Chat {
                    messages: vec![json!({"role":"user","content":"Fresh request"})],
                    tools: vec![],
                    temperature: 0.7,
                    max_tokens: 83,
                    seed: None,
                },
                5,
                None,
            )
            .unwrap();
        agent.backend = Some(backend);
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["max_tokens"], 83,
        "stale request profile leaked"
    );
}

#[test]
fn an_oversized_default_offer_dispatches_only_after_schemas_shrink() {
    use crate::agent::compact::compaction::request_tokens_with_policy;
    use crate::agent::run::tool_exposure::default_offer_tests::with_default_agent;
    use crate::agent::run::turn::completion_tests::response;

    let server = llm::fake::serve(vec![response("Done.", None)]);
    with_default_agent(
        "[endpoint]\nbase_url='https://example.invalid/v1'\n[harness]\nmax_tokens=4096\n[compaction]\nenabled=false\nreserve_tokens=4096\n[verification.review]\nenabled=false",
        |agent| {
            connect(agent, &server.url);
            agent.select_tools(&["git".into()], &[]).unwrap();
            let names = agent.offered();
            let mut schemas = agent.schemas(&names).unwrap();

            // Even the lean instructions alone, with the old schemas and no user history,
            // must exceed the prompt budget. Rebuilding only the prompt cannot pass.
            agent.lean_tools = true;
            agent.rebuild_system();
            let selection = agent.auth_config.selection();
            let tokens = request_tokens_with_policy(
                &Chat {
                    messages: agent.system_messages(),
                    tools: schemas.clone(),
                    temperature: 0.0,
                    max_tokens: 4096,
                    seed: None,
                },
                &agent.config.compaction,
                crate::config::compaction::CompactionIdentity {
                    provider: &selection.provider,
                    model: &selection.model,
                    agent: None,
                },
            );
            agent.context_cap = Some(u32::try_from(tokens).unwrap() + 4096 - 1);
            agent.lean_tools = false;
            agent.rebuild_system();

            let mut backend = agent.backend.take().unwrap();
            let mut engine = crate::agent::run::engine::Engine::default();
            let goal = "Say done";
            agent.push(json!({"role":"user","content":goal})).unwrap();
            let mut turn = agent
                .begin_turn(&mut backend, &names, 5, goal, &mut NullUi)
                .unwrap();
            let retry = agent
                .turn_step(
                    &mut turn,
                    &mut engine,
                    &mut backend,
                    &mut schemas,
                    &mut NullUi,
                )
                .unwrap();
            assert!(agent.lean_tools);
            assert!(matches!(retry, crate::agent::run::engine::Tick::Again));
            agent
                .turn_step(
                    &mut turn,
                    &mut engine,
                    &mut backend,
                    &mut schemas,
                    &mut NullUi,
                )
                .unwrap();
            let requests = completion_requests(&server);
            assert_eq!(requests.len(), 1);
            let sent = offered_names(requests[0]["tools"].as_array().unwrap());
            assert_eq!(sent, offered_names(&schemas));
            assert!(sent.len() < names.len());
            for name in ["read", "edit", "finish", "tool_search", "git"] {
                assert!(sent.iter().any(|entry| entry == name), "{name}: {sent:?}");
            }
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn a_note_answered_by_a_discarded_reply_is_sent_again_with_the_retry() {
    let server = llm::fake::serve(vec![
        // Cut off at the output limit: the reply is discarded and the request retried.
        llm::fake::json(
            200,
            &json!({
                "choices": [{"message": {"role": "assistant", "content": "partial"}, "finish_reason": "length"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 64}
            }),
        ),
        llm::fake::json(
            200,
            &json!({
                "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}]
            }),
        ),
    ]);
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        let mut backend = agent.backend.take().unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &[], 5, "goal", &mut NullUi)
            .unwrap();
        turn.notices
            .push_protocol("[Dial] background job bg1 is no longer running");
        let mut engine = crate::agent::run::engine::Engine::default();
        let mut schemas = Vec::new();
        for _ in 0..2 {
            let _ = agent.turn_step(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                &mut NullUi,
            );
        }
        agent.backend = Some(backend);
    });
    let requests = completion_requests(&server);
    assert!(requests.len() >= 2, "{requests:?}");
    for (index, request) in requests.iter().take(2).enumerate() {
        assert!(
            request["messages"]
                .to_string()
                .contains("bg1 is no longer running"),
            "request {index} lacks the note: {request}"
        );
    }
}

#[test]
fn text_answers_during_an_open_investigation_stop_after_a_bounded_number_of_reminders() {
    let answer = || {
        llm::fake::json(
            200,
            &json!({
                "choices": [{"message": {"role": "assistant", "content": "Here is what I found."}, "finish_reason": "stop"}]
            }),
        )
    };
    let server = llm::fake::serve(vec![answer(), answer(), answer(), answer()]);
    let mut finished = None;
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, &server.url);
        agent.exploration.active = Some(crate::agent::workflow::exploration::Checkpoint {
            owner_id: agent.session.id().to_owned(),
            entry_id: String::new(),
            message_count: 0,
            started_at: String::new(),
            goal: "investigate".into(),
            instruction_entry: None,
        });
        let mut backend = agent.backend.take().unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &[], 50, "goal", &mut NullUi)
            .unwrap();
        let mut engine = crate::agent::run::engine::Engine::default();
        let mut schemas = Vec::new();
        for _ in 0..5 {
            if let Ok(crate::agent::run::engine::Tick::Finished(report)) = agent.turn_step(
                &mut turn,
                &mut engine,
                &mut backend,
                &mut schemas,
                &mut NullUi,
            ) {
                finished = Some(report);
                break;
            }
        }
        agent.backend = Some(backend);
    });
    let report = finished.expect("the turn stops instead of reminding forever");
    assert_eq!(report.outcome, crate::agent::TurnOutcome::Incomplete);
    assert!(
        report.text.contains("without calling rewind"),
        "{}",
        report.text
    );
    let requests = completion_requests(&server);
    let reminders = requests
        .last()
        .map(|request| {
            request["messages"]
                .to_string()
                .matches("Investigation checkpoint is active")
                .count()
        })
        .unwrap_or_default();
    assert_eq!(reminders, 2, "two reminders, then stop");
}

fn prepare(
    agent: &mut Agent<'_>,
    turn: &mut TurnState<'_>,
    backend: &mut Backend,
    step: u32,
) -> PreparedRequest {
    let mut engine = crate::agent::run::engine::Engine::default();
    let mut schemas = Vec::new();
    let mut ui = NullUi;
    let result = agent
        .prepare_step_request(
            &mut StepContext {
                turn,
                engine: &mut engine,
                backend,
                schemas: &mut schemas,
                ui: &mut ui,
            },
            step,
        )
        .unwrap();
    match result {
        ControlFlow::Continue(prepared) => prepared,
        ControlFlow::Break(_) => panic!("the request should fit without a recovery round"),
    }
}

#[cfg(unix)]
#[test]
fn live_job_diagnostic_reaches_the_model_before_exit_without_consuming_output() {
    with_agent("", |agent| {
        let bg = Arc::clone(agent.ext.get("bg").unwrap());
        let ws = agent.workspace().clone().without_sandbox();
        let ctx = ToolContext {
            workspace: &ws,
            config: agent.config,
            mutation_authorizer: None,
            confirm: None,
        };
        let args = json!({"action":"start","name":"stalled","command":"echo 'diagnostic </dial-notice>'; sleep 30", "ready":{"log":"diagnostic"}});
        let start = bg.call(&ctx, &args);
        assert!(!start.is_error, "{}", start.text);
        let mut backend = agent.backend.take().unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &[], 5, "Inspect the job", &mut NullUi)
            .unwrap();
        let absent = prepare(agent, &mut turn, &mut backend, 0);
        assert!(
            !json!(absent.request.messages)
                .to_string()
                .contains("live_job_output")
        );
        let result = agent
            .prepare_step_request(
                &mut StepContext {
                    turn: &mut turn,
                    engine: &mut crate::agent::run::engine::Engine::default(),
                    backend: &mut backend,
                    schemas: &mut vec![bg.schema()],
                    ui: &mut NullUi,
                },
                1,
            )
            .unwrap();
        let ControlFlow::Continue(prepared) = result else {
            panic!("request must fit")
        };
        let note = prepared
            .request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .find(|s| s.contains("live_job_output"))
            .unwrap();
        assert!(
            note.contains("stalled") && note.contains(r"diagnostic <\/dial-notice>"),
            "{note}"
        );
        assert!(bg.has_live_work());
        let read = bg.call(&ctx, &json!({"action":"output","name":"stalled"}));
        assert!(
            read.text.contains("diagnostic </dial-notice>"),
            "{}",
            read.text
        );
        assert!(
            !bg.call(&ctx, &json!({"action":"kill","name":"stalled"}))
                .is_error
        );
        agent.backend = Some(backend);
    });
}

#[test]
fn discovery_advice_survives_priority_and_policy_changing_discarded_requests() {
    with_agent(
        "[compaction]\nenabled=false\n[reliability]\nmeasured_interfaces=true\n",
        |agent| {
            let mut backend = agent.backend.take().unwrap();
            let mut turn = agent
                .begin_turn(&mut backend, &[], 10, "Inspect registry.rs", &mut NullUi)
                .unwrap();
            for pattern in ["name:", "CommandSpec"] {
                agent.step_checks.empty_searches.observe(
                    "grep",
                    &json!({"pattern":pattern,"path":"registry.rs"}),
                    "no matches\n[coverage: walk complete]",
                    false,
                    [0, 0, 0],
                );
            }
            turn.notices.push_advisory(
                crate::agent::run::guidance::Priority::Failure,
                "Resolve the failed prerequisite.",
            );
            let first = prepare(agent, &mut turn, &mut backend, 0);
            assert!(
                !json!(first.request.messages)
                    .to_string()
                    .contains("Discovery recovery")
            );
            turn.reply_received(agent.session.message_count());
            agent
                .push(json!({"role":"assistant","content":"Checking the prerequisite."}))
                .unwrap();

            let batch = prepare(agent, &mut turn, &mut backend, 1);
            assert!(!batch.single_call);
            assert!(
                json!(batch.request.messages)
                    .to_string()
                    .contains(crate::agent::prompts::recovery::DISCOVERY_BATCH)
            );
            // The response is discarded; a contract recovery changes the next request's policy.
            turn.reply_received(agent.session.message_count());
            turn.reliability.consecutive_contract_failures = 2;
            let retry = prepare(agent, &mut turn, &mut backend, 2);
            assert!(retry.single_call);
            let text = json!(retry.request.messages).to_string();
            assert!(
                text.contains(crate::agent::prompts::recovery::DISCOVERY_ONE_CALL),
                "{text}"
            );
            assert!(
                !text.contains(crate::agent::prompts::recovery::DISCOVERY_BATCH),
                "stale batching guidance: {text}"
            );
            turn.reply_received(agent.session.message_count());
            agent
                .push(json!({"role":"assistant","content":"Inspecting the declaration."}))
                .unwrap();
            let delivered = prepare(agent, &mut turn, &mut backend, 3);
            assert!(
                !json!(delivered.request.messages)
                    .to_string()
                    .contains("Discovery recovery")
            );
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn local_repair_brief_survives_preemption_and_discard_but_not_a_kept_reply() {
    use crate::agent::run::guidance::Priority;
    with_agent("[compaction]\nenabled=false", |agent| {
        let mut backend = agent.backend.take().unwrap();
        let mut turn = agent
            .begin_turn(&mut backend, &[], 10, "Fix the score", &mut NullUi)
            .unwrap();
        let has_brief = |p: PreparedRequest| {
            p.request.messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("Local repair brief"))
            })
        };
        assert!(!has_brief(prepare(agent, &mut turn, &mut backend, 0)));
        let call = ToolCall {
            id: "score-run".into(),
            name: "bash".into(),
            malformed_arguments: false,
            arguments: json!({"command":"cargo test score"}),
        };
        let output = ToolOutput::err("AssertionError: expected 5, got 4").with_process(
            crate::runtime::execution::ProcessOutcome {
                started: true,
                exit_code: Some(1),
                signal: None,
                timed_out: false,
                cancelled: false,
                output_complete: true,
            },
        );
        agent.note_verification(&call, &output, &mut NullUi);
        let mut hosted = Backend::new(
            ProviderKind::Endpoint {
                base_url: "https://remote.invalid/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            Default::default(),
            5,
        );
        assert!(!has_brief(prepare(agent, &mut turn, &mut hosted, 1)));
        turn.notices
            .push_advisory(Priority::Blocker, "Resolve the blocker first.");
        assert!(!has_brief(prepare(agent, &mut turn, &mut backend, 1)));
        turn.reply_received(agent.session.message_count());
        agent
            .push(json!({"role":"assistant","content":"Resolving the blocker."}))
            .unwrap();
        assert!(has_brief(prepare(agent, &mut turn, &mut backend, 2)));
        turn.reply_received(agent.session.message_count()); // discarded, no history entry
        assert!(has_brief(prepare(agent, &mut turn, &mut backend, 3)));
        turn.reply_received(agent.session.message_count());
        agent
            .push(json!({"role":"assistant","content":"Repairing the score."}))
            .unwrap();
        assert!(!has_brief(prepare(agent, &mut turn, &mut backend, 4)));
        agent.backend = Some(backend);
    });
}
