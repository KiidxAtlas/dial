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
                        model: "active-test".into(),
                        api_key: None,
                    },
                    Default::default(),
                    5,
                )
                .with_profile(llm::profiles::ModelProfile {
                    provider: "anthropic".into(),
                    model: "active-test".into(),
                    cache_retention: match tier {
                        Tier::Short => llm::profiles::CacheRetention::Short,
                        Tier::Long => llm::profiles::CacheRetention::Long,
                    },
                    ..Default::default()
                }),
            );
            // The live route can differ from the original default (switch/fallback). Turn
            // entry must preserve this route's worker instead of rebuilding for config.model.
            agent.auth_config.model = "active-test".into();
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
                route: WarmRoute::of(&agent.auth_config).unwrap(),
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

#[test]
fn replays_are_bounded_even_when_worker_time_is_unlimited() {
    assert_eq!(replay_timeout(0), REPLAY_TIMEOUT_SECS);
    assert_eq!(replay_timeout(30), 30);
    assert_eq!(replay_timeout(86_400), REPLAY_TIMEOUT_SECS);
}

#[test]
fn same_model_name_on_another_route_replaces_the_armed_worker() {
    type Mutate = fn(&mut Config);
    let cases: [(&str, Mutate); 7] = [
        ("endpoint", |config| {
            config.endpoint = Some(EndpointConfig {
                base_url: "http://127.0.0.1:9/other/v1".into(),
                api_key_env: None,
                allow_insecure: false,
                protocol: None,
                headers: None,
                account_id: None,
            });
        }),
        ("provider", |config| {
            config.provider = Some("other-provider".into())
        }),
        ("request header", |config| {
            config
                .endpoint
                .get_or_insert_with(|| EndpointConfig {
                    base_url: "http://127.0.0.1:9/v1".into(),
                    api_key_env: None,
                    allow_insecure: false,
                    protocol: None,
                    headers: None,
                    account_id: None,
                })
                .headers = Some(vec![llm::model_config::RequestHeader {
                name: "x-cache-route".into(),
                value: llm::model_config::HeaderValue::Literal("alternate".into()),
            }]);
        }),
        ("provider auth", |config| {
            let provider = config.selection().provider;
            config.providers.entry(provider).or_default().auth =
                llm::model_config::AuthMethod::OAuth;
        }),
        ("replay timeout", |config| config.worker_timeout_secs = 17),
        ("explicit profile", |config| {
            let selection = config.selection();
            config.model_profile = Some("tuned".into());
            config.model_profiles.insert(
                "tuned".into(),
                llm::profiles::ModelProfile {
                    provider: selection.provider,
                    model: selection.model,
                    context_window: Some(4096),
                    ..Default::default()
                },
            );
        }),
        ("auto-selected profile", |config| {
            let selection = config.selection();
            config.model_profiles.insert(
                "auto".into(),
                llm::profiles::ModelProfile {
                    provider: selection.provider,
                    model: selection.model,
                    context_window: Some(4096),
                    ..Default::default()
                },
            );
        }),
    ];
    with_agent("[cache]\nwarm='idle'\n", |agent| {
        let live = agent.auth_config.clone();
        let armed = WarmRoute::of(&live).unwrap();
        for (name, mutate) in cases {
            let mut moved = live.clone();
            mutate(&mut moved);
            let next = WarmRoute::of(&moved).unwrap();
            assert_ne!(next, armed, "{name} change must alter route identity");
            // The captured sender is dropped when the old worker exits. Unlike checking the
            // copied identity alone, this proves that the old replay factory was retired.
            let (factory_alive, retired) = mpsc::channel::<()>();
            let handle = Handle::spawn(
                Mode::Idle,
                Box::new(move |_| {
                    let _keep_alive = &factory_alive;
                    anyhow::bail!("this test never replays")
                }),
                Box::new(Instant::now),
            )
            .unwrap();
            let mut slot = Some(CacheWarm {
                handle,
                mode: Mode::Idle,
                route: WarmRoute::of(&live).unwrap(),
                pricing: None,
            });
            CacheWarm::sync(&mut slot, &live).unwrap();
            assert_eq!(retired.try_recv(), Err(mpsc::TryRecvError::Empty));
            CacheWarm::sync(&mut slot, &moved).unwrap();
            assert_eq!(
                retired.recv_timeout(Duration::from_secs(2)),
                Err(mpsc::RecvTimeoutError::Disconnected),
                "{name}: old worker must exit"
            );
            assert_eq!(slot.as_ref().unwrap().route, next, "{name} change");
            CacheWarm::sync(&mut slot, &live).unwrap();
        }
    });
}

#[test]
fn an_exited_worker_is_replaced_before_a_later_request_can_arm() {
    with_agent("[cache]\nwarm='idle'\n", |agent| {
        let config = &agent.auth_config;
        let (factory_alive, retired) = mpsc::channel::<()>();
        let handle = Handle::spawn(
            Mode::Idle,
            Box::new(move |_| {
                let _keep_alive = &factory_alive;
                anyhow::bail!("this test never replays")
            }),
            Box::new(Instant::now),
        )
        .unwrap();
        assert!(handle.send(Event::Shutdown));
        assert_eq!(
            retired.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert!(!handle.is_running());
        let mut slot = Some(CacheWarm {
            handle,
            mode: Mode::Idle,
            route: WarmRoute::of(config).unwrap(),
            pricing: None,
        });
        CacheWarm::sync(&mut slot, config).unwrap();
        assert!(slot.as_ref().unwrap().handle.is_running());
    });
}
