use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::llm::profiles::{ModelCapabilities, ModelProfile, ReasoningEffort};

fn profile() -> ModelProfile {
    ModelProfile {
        capabilities: ModelCapabilities {
            reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::Medium],
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn auto_thinking_waits_for_a_supported_request_and_runs_once_per_turn() {
    let reply = json!({"choices":[{"message":{"role":"assistant","content":"{\"choice\":\"medium\"}"},"finish_reason":"stop"}]});
    // Empty script sets can be inspected without waiting for a future request.
    let server = llm::fake::serve(vec![]);
    with_agent(
        "thinking_auto=true\n[assistance]\nlean_local=false",
        |agent| {
            agent.auth_config.provider = Some("lm-studio".into());
            agent.auth_config.endpoint = Some(
                serde_json::from_value(json!({
                    "base_url": format!("{}/v1", server.url)
                }))
                .unwrap(),
            );
            let mut backend = agent.backend.take().unwrap();
            let mut ui = NullUi;
            let mut turn = agent
                .begin_turn(&mut backend, &[], 10, "fix the bug", &mut ui)
                .unwrap();
            assert!(turn.pending_thinking.is_some());
            assert!(
                server.requests().is_empty(),
                "setup must not make a judgment request"
            );
            agent.judge_turn_effort(&mut turn, None, &mut ui);
            agent.judge_turn_effort(&mut turn, Some(&ModelProfile::default()), &mut ui);
            assert!(
                server.requests().is_empty(),
                "unsupported requests cannot use a judgment"
            );
            assert!(
                turn.pending_thinking.is_some(),
                "a supported fallback must retain its opportunity"
            );
            let server = llm::fake::serve(vec![llm::fake::json(200, &reply)]);
            agent.auth_config.endpoint.as_mut().unwrap().base_url = format!("{}/v1", server.url);
            agent.judge_turn_effort(&mut turn, Some(&profile()), &mut ui);
            assert_eq!(server.requests().len(), 1);
            assert_eq!(turn.turn_effort, Some(ReasoningEffort::Medium));
            assert_eq!(agent.last_auto_thinking.as_deref(), Some("medium"));
            // Ordinary steps and repair/stop-hook rounds retain this same turn state.
            for _ in 0..3 {
                agent.judge_turn_effort(&mut turn, Some(&profile()), &mut ui);
            }
            assert_eq!(server.requests().len(), 1);
            agent.backend = Some(backend);
        },
    );
}

#[test]
fn auto_thinking_exclusions_and_failed_judgments_do_not_retry() {
    with_agent("thinking_auto=true", |agent| {
        let mut backend = agent.backend.take().unwrap();
        let mut ui = NullUi;
        for external in [false, true] {
            agent.hermetic = !external;
            agent.modes.launch.external_thinking = external;
            let mut turn = agent
                .begin_turn(&mut backend, &[], 10, "question", &mut ui)
                .unwrap();
            assert!(turn.pending_thinking.is_none());
            agent.judge_turn_effort(&mut turn, Some(&profile()), &mut ui);
            assert!(turn.turn_effort.is_none());
        }
        agent.hermetic = false;
        agent.modes.launch.external_thinking = false;
        let mut turn = agent
            .begin_turn(&mut backend, &[], 10, "question", &mut ui)
            .unwrap();
        // Cancellation fails before opening the cache or contacting any provider.
        agent
            .interrupt
            .store(true, std::sync::atomic::Ordering::SeqCst);
        agent.judge_turn_effort(&mut turn, Some(&profile()), &mut ui);
        assert!(turn.pending_thinking.is_none());
        assert!(turn.turn_effort.is_none());
        agent.judge_turn_effort(&mut turn, Some(&profile()), &mut ui);
        agent.backend = Some(backend);
    });
}
