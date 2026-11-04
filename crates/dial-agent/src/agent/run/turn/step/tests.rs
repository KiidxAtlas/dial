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
        turn.pending_nudges.push(json!({
            "role": "user",
            "dial_transient": true,
            "content": "[Dial] background job bg1 is no longer running"
        }));
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

#[test]
fn discovery_advice_survives_priority_and_policy_changing_discarded_requests() {
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
            turn.pending_nudges.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Failure,
                "Resolve the failed prerequisite.",
            ));
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
                    .contains("Gather unrelated docs")
            );
            // The response is discarded; a contract recovery changes the next request's policy.
            turn.reply_received(agent.session.message_count());
            turn.reliability.consecutive_contract_failures = 2;
            let retry = prepare(agent, &mut turn, &mut backend, 2);
            assert!(retry.single_call);
            let text = json!(retry.request.messages).to_string();
            assert!(text.contains("inspect that declaration next"), "{text}");
            assert!(
                !text.contains("Gather unrelated docs"),
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
