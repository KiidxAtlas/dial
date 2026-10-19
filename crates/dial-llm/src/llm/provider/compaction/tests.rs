use super::*;
use llm::profiles::ModelProfile;

#[test]
fn compaction_summary_preserves_explicit_chatgpt_override_errors() {
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            model: "account-model".into(),
            user_dir: Default::default(),
        },
        Default::default(),
        5,
    );
    assert!(backend.compaction_summary_profile(64).is_none());
    let request = Chat {
        messages: vec![json!({"role":"user","content":"Summarize."})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    for settings in ["max_tokens=128", "temperature=0.5", "top_p=0.9"] {
        let profile: ModelProfile = toml::from_str(&format!(
            "provider='chatgpt'\nmodel='account-model'\n{settings}\n[capabilities]\ntemperature=true\ntop_p=true"
        ))
        .unwrap();
        backend.next_request_profile(Some(profile));
        let profile = backend.compaction_summary_profile(request.max_tokens);
        let error =
            llm::responses::request_codex("account-model", &request, profile.as_ref()).unwrap_err();
        assert!(
            error.to_string().contains("ChatGPT plan usage"),
            "{error:#}"
        );
    }
}
