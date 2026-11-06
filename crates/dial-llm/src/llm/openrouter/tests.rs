use super::*;
use crate::llm::{
    Chat, fake,
    model_config::ApiProtocol,
    profiles::ModelProfile,
    provider::{Backend, ProviderKind},
};
use serde_json::json;

fn preferences() -> Value {
    json!({"sort":"latency", "order":["deepinfra/turbo", "google-vertex/us-east5"],
        "only":["deepinfra", "google-vertex"], "ignore":["azure"],
        "allow_fallbacks":false, "require_parameters":true, "data_collection":"deny", "zdr":true,
        "max_price":{"prompt":1.0,"completion":2.0,"request":0.01,"image":0.02}})
}
fn profile() -> ModelProfile {
    ModelProfile {
        openrouter_routing: Some(serde_json::from_value(preferences()).unwrap()),
        ..ModelProfile::minimal("openrouter", "test/model")
    }
}
fn chat() -> Chat {
    Chat {
        messages: vec![json!({"role":"user","content":"hi"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 32,
        seed: None,
    }
}

#[test]
fn routing_roundtrips_and_defaults_do_not_change_provider_preferences() {
    let profile = profile();
    profile.validate().unwrap();
    let encoded = toml::to_string(&profile).unwrap();
    let decoded: ModelProfile = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded.openrouter_routing, profile.openrouter_routing);
    assert!(
        serde_json::to_value(ModelProfile::default())
            .unwrap()
            .get("openrouter_routing")
            .is_none()
    );
    let mut body = json!({"model":"test/model"});
    apply(
        &mut body,
        &ModelProfile::minimal("openrouter", "test/model"),
    )
    .unwrap();
    assert!(body.get("provider").is_none());
    let mut empty = ModelProfile::minimal("openrouter", "test/model");
    empty.openrouter_routing = Some(Routing::default());
    apply(&mut body, &empty).unwrap();
    assert!(body.get("provider").is_none());
    for sort in ["price", "latency", "throughput"] {
        let routing: Routing = serde_json::from_value(json!({"sort":sort,"data_collection":"allow","zdr":false,"require_parameters":false,"allow_fallbacks":true})).unwrap();
        routing.validate("openrouter").unwrap();
        assert_eq!(
            serde_json::to_value(routing).unwrap(),
            json!({"sort":sort,"data_collection":"allow","zdr":false,"require_parameters":false,"allow_fallbacks":true})
        );
    }
}

#[test]
fn unknown_fields_types_and_invalid_routing_fail_closed() {
    for value in [
        json!({"sort":"fast"}),
        json!({"data_collection":"private"}),
        json!({"zdr":"true"}),
        json!({"extra_body":{}}),
        json!({"max_price":{"tokens":1}}),
    ] {
        assert!(
            serde_json::from_value::<Routing>(value.clone()).is_err(),
            "{value}"
        );
    }
    for value in [
        json!({"only":[]}),
        json!({"order":[""]}),
        json!({"ignore":[" deepinfra"]}),
        json!({"only":["a//b"]}),
        json!({"order":["a","a"]}),
        json!({"max_price":{}}),
        json!({"max_price":{"prompt":-1}}),
    ] {
        let routing: Routing = serde_json::from_value(value.clone()).unwrap();
        assert!(routing.validate("openrouter").is_err(), "{value}");
    }
    for price in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            Routing {
                max_price: Some(MaxPrice {
                    prompt: Some(price),
                    ..Default::default()
                }),
                ..Default::default()
            }
            .validate("openrouter")
            .is_err()
        );
    }
    Routing {
        max_price: Some(MaxPrice {
            prompt: Some(0.0),
            ..Default::default()
        }),
        ..Default::default()
    }
    .validate("openrouter")
    .unwrap();
    for provider in ["openai", "openrouter-custom", "OpenRouter", "anthropic"] {
        let mut profile = profile();
        profile.provider = provider.into();
        assert!(profile.validate().is_err(), "{provider}");
        assert!(apply(&mut json!({}), &profile).is_err(), "{provider}");
    }
}

#[test]
fn model_profiles_toml_survives_role_selection_and_request_transformations() {
    let config: crate::config::types::Config = toml::from_str(
        r#"
provider = "openrouter"
model = "test/model"
model_profile = "private"
[model_profiles.private]
provider = "openrouter"
model = "test/model"
[model_profiles.private.openrouter_routing]
sort = "throughput"
only = ["deepinfra/turbo"]
allow_fallbacks = false
require_parameters = true
data_collection = "deny"
zdr = true
[model_profiles.private.openrouter_routing.max_price]
prompt = 1.0
completion = 2.0
[model_roles.worker]
provider = "openrouter"
model = "test/model"
"#,
    )
    .unwrap();
    let expected = config
        .active_model_profile()
        .unwrap()
        .openrouter_routing
        .clone();
    for config in [
        config.clone(),
        config.for_role("worker"),
        config.for_role("reviewer"),
        config.with_named_model_profile("private").unwrap(),
        config.with_selection(&config.selection()),
    ] {
        let profile = config.active_model_profile().cloned().unwrap();
        let profile = crate::llm::profiles::request_tiers_for(
            Some(profile),
            &config.selection(),
            "none",
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(profile.openrouter_routing, expected);
    }
}

#[test]
fn backend_chat_and_responses_keep_every_control_across_transient_retries() {
    for protocol in [ApiProtocol::ChatCompletions, ApiProtocol::Responses] {
        let success = match protocol {
            ApiProtocol::Responses => fake::Script::Sse(format!(
                "data: {}\n\n",
                json!({"type":"response.completed","response":{"status":"completed","output":[]}})
            )),
            _ => fake::json(
                200,
                &json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}]}),
            ),
        };
        let server = fake::serve(vec![
            fake::json(503, &json!({"error":{"message":"unavailable"}})),
            success,
        ]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: server.url.clone(),
                model: "test/model".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        )
        .with_protocol(Some(protocol))
        .with_profile(profile());
        backend.chat(None, &chat(), 5, None).unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{protocol:?}");
        for request in requests {
            assert_eq!(
                fake::body(&request)["provider"],
                preferences(),
                "{protocol:?}"
            );
        }
    }
}

#[test]
fn incompatible_protocol_rejects_routing_before_dispatch() {
    let server = fake::serve(vec![]);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: server.url.clone(),
            model: "test/model".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    )
    .with_protocol(Some(ApiProtocol::Gemini))
    .with_profile(profile());
    let error = backend.chat(None, &chat(), 5, None).unwrap_err();
    assert!(
        error.to_string().contains("OpenRouter routing requires"),
        "{error:#}"
    );
    assert!(server.requests().is_empty());
}
