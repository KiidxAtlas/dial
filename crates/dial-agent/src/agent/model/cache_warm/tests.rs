#![cfg(unix)]

use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::llm::cache_warmer::Tier;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

struct Capture(mpsc::Sender<Chat>);

impl Replayer for Capture {
    fn replay(&mut self, chat: &Chat, _: Option<&Scope>) -> Result<Option<u64>> {
        self.0.send(chat.clone()).unwrap();
        Ok(Some(100_000))
    }
}

#[test]
fn text_only_final_arms_exact_request_and_respects_effective_retention() {
    for (tier, rewrite) in [
        (Tier::Short, false),
        (Tier::Long, false),
        (Tier::Short, true),
    ] {
        let replacement = json!({
            "messages": [{"role":"user","content":"Give the rewritten answer"}],
            "tools": [], "temperature": 0.37, "max_tokens": 73, "seed": 1234
        });
        let mut overlay = "[cache]\nwarm='idle'\n[compaction]\nenabled=false\n".to_owned();
        if rewrite {
            overlay.push_str(&format!(
                "[[hooks]]\nevent='before_provider_request'\ncommand=['sh','-c', '''printf '%s' '{}' ''']\n",
                json!({"payload": replacement})
            ));
        }
        // The last hook captures the actual provider-neutral request, after any rewrite.
        overlay.push_str("[[hooks]]\nevent='before_provider_request'\ncommand=['sh','-c','cat > sent-request.json']\n");
        let server = llm::fake::serve(vec![llm::fake::json(
            200,
            &json!({
                "content":[{"type":"text","text":"The answer is ready."}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":100_000,"output_tokens":5}
            }),
        )]);
        with_agent(&overlay, |agent| {
            agent.backend = Some(
                Backend::new(
                    ProviderKind::AnthropicEndpoint {
                        base_url: format!("{}/v1", server.url),
                        model: "test".into(),
                        api_key: None,
                    },
                    Default::default(),
                    5,
                )
                .with_profile(llm::profiles::ModelProfile {
                    provider: "anthropic".into(),
                    model: "test".into(),
                    cache_retention: match tier {
                        Tier::Short => llm::profiles::CacheRetention::Short,
                        Tier::Long => llm::profiles::CacheRetention::Long,
                    },
                    ..Default::default()
                }),
            );
            let start = Instant::now();
            let now = Arc::new(Mutex::new(start));
            let clock_now = now.clone();
            let (tx, rx) = mpsc::channel();
            let handle = Handle::spawn(
                Mode::Idle,
                Box::new(move |_| Ok(Box::new(Capture(tx.clone())))),
                Box::new(move || *clock_now.lock().unwrap()),
            )
            .unwrap();
            agent.cache_warm = Some(CacheWarm {
                handle,
                mode: Mode::Idle,
                model: agent.config.model.clone(),
                pricing: Some(TokenPricing {
                    prompt: 3.0 / 1_000_000.0,
                    completion: 15.0 / 1_000_000.0,
                    cache_read: Some(0.3 / 1_000_000.0),
                    cache_write: Some(3.75 / 1_000_000.0),
                }),
            });
            let report = agent.turn("Give the answer", &mut NullUi).unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed);
            // Let the worker consume Settled and RunIdle before advancing its injected clock.
            assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
            *now.lock().unwrap() = start + Duration::from_secs(241);
            if tier == Tier::Long {
                assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
                *now.lock().unwrap() = start + Duration::from_secs(3541);
            }
            let replay = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("idle final prefix is warmed");
            assert_eq!(replay.max_tokens, 1);
            assert!(replay.messages.iter().any(|message| message["content"]
                == if rewrite {
                    "Give the rewritten answer"
                } else {
                    "Give the answer"
                }));
            assert!(
                !replay
                    .messages
                    .iter()
                    .any(|message| message["content"] == "The answer is ready.")
            );
            let captured: Value = serde_json::from_slice(
                &std::fs::read(agent.workspace.root().join("sent-request.json")).unwrap(),
            )
            .unwrap();
            let expected = &captured["request"];
            if rewrite {
                assert_eq!(expected, &replacement);
            }
            assert_eq!(replay.messages, *expected["messages"].as_array().unwrap());
            assert_eq!(replay.tools, *expected["tools"].as_array().unwrap());
            assert_eq!(
                replay.temperature,
                expected["temperature"].as_f64().unwrap()
            );
            assert_eq!(replay.seed, expected["seed"].as_u64());
            // This fixture's profile does not advertise temperature support; native
            // serialization deliberately omits it. Warming retains the neutral Chat.
            let sent = llm::fake::body(&server.requests()[0]);
            assert!(sent.get("temperature").is_none());
            assert_eq!(sent["max_tokens"], expected["max_tokens"]);
        });
    }
}
