use super::*;

fn resolve(text: &str) -> Result<Config> {
    let project = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned())?;
    Config::from_table(&layout, toml::from_str(text)?)
}

#[test]
fn bundled_provider_ids_resolve_without_user_endpoint_configuration() {
    let together =
        resolve("provider='together'\nmodel='meta-llama/Llama-3.3-70B-Instruct-Turbo'\n").unwrap();
    let endpoint = together.endpoint.unwrap();
    assert_eq!(endpoint.base_url, "https://api.together.xyz/v1");
    assert_eq!(endpoint.api_key_env.as_deref(), Some("TOGETHER_API_KEY"));
    let selection = crate::llm::model_config::ModelSelection {
        provider: "fireworks".into(),
        model: "accounts/fireworks/models/llama-v3p1-70b-instruct".into(),
        ..Default::default()
    };
    let configured = resolve("model='local'\n")
        .unwrap()
        .with_selection(&selection);
    assert_eq!(
        configured.endpoint.unwrap().base_url,
        "https://api.fireworks.ai/inference/v1"
    );
}
#[test]
fn context_window_accepts_auto_and_numeric_values() {
    assert_eq!(
        resolve("context_window='auto'\n").unwrap().context_window,
        32768
    );
    assert_eq!(
        resolve("context_window=8192\n").unwrap().context_window,
        8192
    );
}

#[test]
fn context_auto_and_family_caps_follow_model_capacity_without_loading_weights() {
    use crate::llm::{chatgpt, model_config::ModelSelection};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    chatgpt::test_account(home.path(), false);
    chatgpt::test_catalog(
        home.path(),
        &serde_json::json!({"models":[
            {"slug":"large","context_window":272000},
            {"slug":"small","context_window":16000}
        ]}),
    );
    let load = |text: &str| Config::from_table(&layout, toml::from_str(text).unwrap()).unwrap();
    let auto = load("provider='chatgpt'\nmodel='large'\ncontext_window='auto'\n");
    assert_eq!(auto.context_window, 272000);
    assert_eq!(
        toml::Table::try_from(&auto).unwrap()["context_window"].as_str(),
        Some("auto")
    );
    let capped = load(
        "provider='chatgpt'\nmodel='large'\ncloud_context_window=64000\nlocal_context_window=8192\n",
    );
    assert_eq!(capped.context_window, 64000);
    let small = ModelSelection {
        provider: "chatgpt".into(),
        model: "small".into(),
        ..Default::default()
    };
    assert_eq!(capped.with_selection(&small).context_window, 16000);
    let unknown = ModelSelection {
        provider: "chatgpt".into(),
        model: "unknown".into(),
        ..Default::default()
    };
    assert_eq!(capped.with_selection(&unknown).context_window, 64000);
    let override_main = load(
        "provider='chatgpt'\nmodel='large'\ncontext_window=96000\ncloud_context_window=64000\n",
    );
    assert_eq!(override_main.context_window, 96000);
    assert_eq!(override_main.with_selection(&small).context_window, 16000);
    assert_eq!(
        load("provider='chatgpt'\nmodel='small'\ncontext_window=96000\n").context_window,
        16000
    );

    let model = home.path().join("local-model");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(
        model.join("config.json"),
        r#"{"max_position_embeddings":131072}"#,
    )
    .unwrap();
    std::fs::write(model.join("model.safetensors"), b"metadata-only fixture").unwrap();
    let local = ModelSelection {
        model: model.to_string_lossy().into(),
        ..Default::default()
    };
    assert_eq!(auto.with_selection(&local).context_window, 131072);
    assert_eq!(capped.with_selection(&local).context_window, 8192);
    let server = ModelSelection {
        provider: "lm-studio".into(),
        model: "unknown".into(),
        base_url: Some("http://127.0.0.1:1234/v1".into()),
        ..Default::default()
    };
    assert_eq!(capped.with_selection(&server).context_window, 8192);
    assert_eq!(
        std::fs::read_dir(home.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("process"))
            .count(),
        0
    );
}

#[test]
fn context_settings_reject_invalid_caps_and_accept_legacy_numbers() {
    for key in [
        "context_window",
        "cloud_context_window",
        "local_context_window",
    ] {
        for invalid in ["2047", "2097153", "-1", "'banana'", "2.5"] {
            assert!(
                resolve(&format!("{key}={invalid}\n")).is_err(),
                "{key}={invalid}"
            );
        }
        assert!(resolve(&format!("{key}='auto'\n")).is_ok());
        assert!(resolve(&format!("{key}=64000\n")).is_ok());
        assert!(resolve(&format!("{key}=1050000\n")).is_ok());
        assert!(resolve(&format!("{key}=2000000\n")).is_ok());
    }
}

#[test]
fn profile_context_size_overrides_shared_caps_and_respects_capacity() {
    let config = resolve("provider='anthropic'\nmodel='claude-opus-5-5'\ncontext_window=32768\ncloud_context_window=64000\nmodel_profile='main'\n[model_profiles.main]\nprovider='anthropic'\nmodel='claude-opus-5-5'\ncontext_window=200000\n").unwrap();
    assert_eq!(config.context_window, 200_000);
    assert_eq!(config.requested_context_window(), Some(200_000));
    let mut selection = config.selection();
    selection.context_window = None;
    assert_eq!(config.with_selection(&selection).context_window, 200_000);

    let mut larger = config.clone();
    larger
        .model_profiles
        .get_mut("main")
        .unwrap()
        .context_window = Some(2_000_000);
    assert_eq!(larger.with_selection(&selection).context_window, 1_000_000);

    let mut reset = config.clone();
    reset.model_profiles.get_mut("main").unwrap().context_window = None;
    assert_eq!(reset.requested_context_window(), Some(32768));
    assert_eq!(reset.with_selection(&selection).context_window, 64000);

    selection.model = "another-model".into();
    let switched = config.with_selection(&selection);
    assert_eq!(switched.context_window, 64000);
    assert_eq!(switched.requested_context_window(), Some(64000));
    assert!(switched.active_model_profile().is_none());
}

#[test]
fn claude_context_resolves_on_load_and_selection_without_overriding_user_limits() {
    let config = resolve("provider='anthropic'\nmodel='claude-opus-5-5'\n").unwrap();
    assert_eq!(config.context_window, 1_000_000);
    let capped =
        resolve("provider='anthropic'\nmodel='claude-opus-5-5'\ncontext_window=32768\n").unwrap();
    assert_eq!(capped.context_window, 32768);
    let mut selection = crate::llm::model_config::ModelSelection {
        provider: "anthropic".into(),
        model: "claude-opus-5-5".into(),
        base_url: Some(crate::llm::anthropic::BASE_URL.into()),
        ..Default::default()
    };
    assert_eq!(capped.with_selection(&selection).context_window, 1_000_000);
    selection.context_window = Some(200_000);
    assert_eq!(config.with_selection(&selection).context_window, 200_000);
    selection.base_url = Some("https://proxy.example/v1".into());
    selection.context_window = None;
    assert_eq!(config.with_selection(&selection).context_window, 32768);
}

#[test]
fn unassigned_children_inherit_parent_controls_and_profile() {
    let parent = resolve("provider='chatgpt'\nmodel='account-model'\nworker_timeout_secs=91\nremote_model_idle_timeout_secs=73\nmodel_profile='main'\n[model_profiles.main]\nprovider='chatgpt'\nmodel='account-model'\nmax_tokens=1234\n[harness]\nmax_steps=0\ntemperature=0.3\n").unwrap();
    let child = parent.for_role("tester");
    assert_eq!(child.model, parent.model);
    assert_eq!(child.model_profile, parent.model_profile);
    assert_eq!(child.worker_timeout_secs, 91);
    assert_eq!(child.remote_model_idle_timeout_secs, 73);
    assert_eq!(child.harness.max_steps, 0);
    assert_eq!(child.harness.temperature, parent.harness.temperature);
    assert_eq!(
        child.active_model_profile().unwrap().max_tokens,
        parent.active_model_profile().unwrap().max_tokens
    );
}

#[test]
fn subscription_main_and_roles_resolve_without_local_compute_or_api_keys() {
    let config = resolve("provider='chatgpt'\nmodel='account-model'\n[model_roles.small]\nprovider='chatgpt'\nmodel='account-small'\n[codebase]\narchitecture_model=''\n").unwrap();
    assert!(!config.uses_local_compute());
    for resolved in [config.clone(), config.for_role("small")] {
        let endpoint = resolved.endpoint.unwrap();
        assert_eq!(endpoint.base_url, crate::llm::chatgpt::BASE_URL);
        assert!(endpoint.api_key_env.is_none());
    }
}
#[test]
fn migrates_saved_chatgpt_api_endpoint_to_codex_backend() {
    let main=resolve("provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://api.openai.com/v1'\n").unwrap();
    assert_eq!(
        main.endpoint.unwrap().base_url,
        crate::llm::chatgpt::BASE_URL
    );
    let provider = resolve("[providers.chatgpt]\nbase_url='https://api.openai.com/v1'\n").unwrap();
    assert_eq!(
        provider.providers["chatgpt"].base_url,
        crate::llm::chatgpt::BASE_URL
    );
    let role=resolve("model='local'\nprovider='mlx'\n[model_roles.small]\nprovider='chatgpt'\nmodel='account-model'\nbase_url='https://api.openai.com/v1'\n").unwrap();
    assert_eq!(
        role.model_roles["small"].base_url.as_deref(),
        Some(crate::llm::chatgpt::BASE_URL)
    );
}

#[test]
fn budget_caps_must_be_positive_and_parse_from_toml() {
    let config = resolve("provider='chatgpt'\nmodel='m'\n[budget]\nmax_session_cost_usd=2.5\nmax_turn_cost_usd=0.5\n").unwrap();
    assert_eq!(
        (
            config.budget.max_session_cost_usd,
            config.budget.max_turn_cost_usd
        ),
        (Some(2.5), Some(0.5))
    );
    config.validate().unwrap();
    let mut bad = config;
    bad.budget.max_turn_cost_usd = Some(0.0);
    assert!(bad.validate().is_ok());
    bad.budget.max_turn_cost_usd = Some(f64::NAN);
    assert!(bad.validate().is_err());
}

#[test]
fn laya_judge_must_name_the_shared_decision_checkpoint() {
    let mut config = resolve("provider='chatgpt'\nmodel='m'\n").unwrap();
    config.model_roles.insert(
        "judge".into(),
        crate::llm::model_config::ModelSelection::new(
            crate::config::laya::PROVIDER,
            crate::config::laya::JUDGE_MODEL,
        ),
    );
    config.validate().unwrap();
    config.model_roles.get_mut("judge").unwrap().model = "/other/checkpoint".into();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("decision role's checkpoint"), "{error}");
}

#[test]
fn architecture_synthesis_defaults_local_and_an_assigned_small_role_overrides_it() {
    let mut config = resolve("provider='chatgpt'\nmodel='account-model'\n").unwrap();
    let local = config.architecture_config();
    assert_eq!(local.model, "mlx-community/Qwen2.5-3B-Instruct-4bit");
    assert_eq!(local.provider.as_deref(), Some("mlx"));
    assert!(local.endpoint.is_none());
    assert_eq!(local.context_window, 32768);
    // A cloud main model holds no local compute; the local synthesis default must not make turns wait for training.
    assert!(!config.uses_local_compute());
    config.codebase.architecture_model.clear();
    assert_eq!(config.architecture_config().selection(), config.selection());
    assert!(!config.uses_local_compute());
    config.codebase.architecture_model = "local-summary".into();
    config
        .model_roles
        .insert("small".into(), config.selection());
    // An explicit role resolves the provider's builtin protocol (`selection_endpoint`), while the
    // main selection leaves it implicit; both mean the same Responses protocol for chatgpt.
    let mut expected = config.selection();
    expected.protocol = expected
        .protocol
        .or(Some(crate::llm::model_config::ApiProtocol::builtin(
            "chatgpt",
        )));
    assert_eq!(config.architecture_config().selection(), expected);
    assert_eq!(config.for_role("small").selection(), expected);
    config.model_roles.clear();
    config.codebase.synthesize_architecture = false;
    assert!(!config.uses_local_compute());
}

#[test]
fn a_local_small_role_holds_the_compute_lock_for_a_cloud_session() {
    let cloud = "provider='anthropic'\nmodel='claude-sonnet-5-5'\n[endpoint]\nbase_url='https://api.anthropic.com/v1'\n";
    let small = "[model_roles.small]\nprovider='mlx'\nmodel='local-summary'\n";
    assert!(
        resolve(&format!("{cloud}{small}"))
            .unwrap()
            .uses_local_compute()
    );
}

#[test]
fn compute_locks_can_be_disabled_without_changing_local_routing() {
    let mut config = resolve("provider='mlx'\nmodel='local-model'\ncompute_locks=false\n").unwrap();
    assert!(config.uses_local_compute());
    assert!(!config.requires_compute_lock());
    config.compute_locks = true;
    assert!(config.requires_compute_lock());
}

#[test]
fn laya_roles_never_require_compute_locks_for_cloud_sessions() {
    let mut config = resolve("provider='chatgpt'\nmodel='account-model'\n").unwrap();
    for role in ["decision", "judge"] {
        config.model_roles.insert(
            role.into(),
            crate::llm::model_config::ModelSelection::new(
                crate::config::laya::PROVIDER,
                crate::config::laya::JUDGE_MODEL,
            ),
        );
    }
    assert!(!config.uses_local_compute());
    assert!(!config.requires_compute_lock());
    assert!(!config.memory_role_is_local("judge", None));
}

#[test]
fn subscription_credentials_cannot_be_redirected_or_replaced_with_api_keys() {
    for text in [
        "provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://example.com/v1'\n",
        "provider='chatgpt'\nmodel='account-model'\n[endpoint]\nbase_url='https://api.openai.com/v1'\napi_key_env='OPENAI_API_KEY'\n",
        "[model_roles.small]\nprovider='chatgpt'\nmodel='account-model'\nbase_url='https://example.com/v1'\n",
        "[model_roles.small]\nprovider='chatgpt'\nmodel='account-model'\napi_key_env='OPENAI_API_KEY'\n",
        "[providers.chatgpt]\nbase_url='https://example.com/v1'\n",
    ] {
        assert!(
            resolve(text).is_err(),
            "accepted unsafe subscription configuration"
        );
    }
}
