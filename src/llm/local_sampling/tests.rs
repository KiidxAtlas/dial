use super::*;
#[test]
fn lmstudio_profiles_round_trip_and_use_supported_wire_names() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile};
    let mut profile = ModelProfile {
        provider: "lm-studio".into(),
        model: "qwen".into(),
        capabilities: ModelCapabilities {
            seed: true,
            ..Default::default()
        },
        local_sampling: Some(LocalSampling {
            top_k: Some(40),
            min_p: Some(0.05),
            seed: Some(7),
            repetition_penalty: Some(1.1),
            presence_penalty: Some(0.2),
            frequency_penalty: Some(0.3),
            ..Default::default()
        }),
        ..Default::default()
    };
    profile.validate().unwrap();
    let encoded = toml::to_string(&profile).unwrap();
    profile = toml::from_str(&encoded).unwrap();
    let mut body = serde_json::json!({"max_tokens":32});
    profile.apply_openai(&mut body);
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["min_p"], 0.05);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["repeat_penalty"], 1.1);
    assert_eq!(body["presence_penalty"], 0.2);
    assert_eq!(body["frequency_penalty"], 0.3);
    assert!(body.get("repetition_penalty").is_none());
    profile.local_sampling.as_mut().unwrap().xtc_probability = Some(0.1);
    assert!(
        profile
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not supported by LM Studio")
    );
    let mut body = serde_json::json!({"max_tokens":32});
    profile.apply_openai(&mut body);
    assert!(body.get("xtc_probability").is_none());
    profile.provider = "openai".into();
    assert!(profile.validate().is_err());
    let mut body = serde_json::json!({"max_tokens":32});
    profile.apply_openai(&mut body);
    assert!(body.get("top_k").is_none());
    assert!(body.get("repeat_penalty").is_none());
}

#[test]
fn local_sampling_validates_and_omits_unspecified_options() {
    let sampling = LocalSampling {
        top_k: Some(40),
        min_p: Some(0.05),
        seed: Some(7),
        ..Default::default()
    };
    sampling.validate().unwrap();
    let mut body = serde_json::json!({"temperature":0.2});
    sampling.apply(&mut body);
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["seed"], 7);
    assert!(body.get("presence_penalty").is_none());
    assert!(
        LocalSampling {
            min_p: Some(1.1),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}
#[test]
fn profile_controls_round_trip_and_never_leak_to_cloud_requests() {
    use crate::llm::profiles::{ModelCapabilities, ModelProfile};
    let mut profile = ModelProfile {
        provider: "mlx".into(),
        model: "test-model".into(),
        capabilities: ModelCapabilities {
            seed: true,
            ..Default::default()
        },
        local_sampling: Some(LocalSampling {
            top_k: Some(40),
            seed: Some(7),
            ..Default::default()
        }),
        ..Default::default()
    };
    profile.validate().unwrap();
    let encoded = toml::to_string(&profile).unwrap();
    let restored: ModelProfile = toml::from_str(&encoded).unwrap();
    assert_eq!(restored.local_sampling.unwrap().top_k, Some(40));
    let mut body = serde_json::json!({"max_tokens":100});
    profile.apply_openai(&mut body);
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["seed"], 7);
    profile.provider = "openai".into();
    assert!(profile.validate().is_err());
    let mut body = serde_json::json!({"max_tokens":100});
    profile.apply_openai(&mut body);
    assert!(body.get("top_k").is_none());
    assert!(body.get("seed").is_none());
}
