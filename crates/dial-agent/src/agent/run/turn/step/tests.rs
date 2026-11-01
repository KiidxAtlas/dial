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
