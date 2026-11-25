use super::*;

#[test]
fn prompt_announces_only_its_id_and_takes_the_answer() {
    let state = Arc::new(AuthState::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let (announced_tx, announced_rx) = mpsc::channel();
    let worker_state = state.clone();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || {
        worker_state.ask(
            |prompt_id| {
                announced_tx.send(prompt_id.to_owned()).unwrap();
                true
            },
            &worker_cancel,
            || false,
        )
    });
    let prompt_id = announced_rx.recv().unwrap();
    assert!(!prompt_id.contains("secret"));
    state.answer(&prompt_id, "secret-code".into()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), "secret-code");
}

#[test]
fn answers_for_unknown_prompts_are_refused() {
    let state = AuthState::default();
    assert!(state.answer("p-missing", "code".into()).is_err());
}

#[test]
fn cancel_ends_a_waiting_prompt() {
    let state = AuthState::default();
    let cancel = AtomicBool::new(true);
    let result = state.ask(|_| true, &cancel, || false);
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}

#[test]
fn a_prompt_that_cannot_be_announced_fails_immediately() {
    let state = AuthState::default();
    let cancel = AtomicBool::new(false);
    let result = state.ask(|_| false, &cancel, || false);
    assert!(result.is_err());
}

#[test]
fn a_gone_client_ends_a_waiting_prompt() {
    let state = AuthState::default();
    let cancel = AtomicBool::new(false);
    let result = state.ask(|_| true, &cancel, || true);
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}

#[test]
fn code_submission_needs_a_prompt_id_and_a_string_code() {
    let state = AuthState::default();
    assert!(submit_code(&state, &json!({"code": "1"})).is_err());
    assert!(submit_code(&state, &json!({"prompt_id": "p1", "code": 1})).is_err());
    assert!(submit_code(&state, &json!({"prompt_id": "p1", "code": "1"})).is_err());
}

#[test]
fn cancel_without_a_running_sign_in_is_refused() {
    let state = AuthState::default();
    assert!(cancel_login(&state, &json!({"provider": "copilot"})).is_err());
}

#[test]
fn provider_listing_is_keyed_by_provider_id() {
    let listing = providers(Path::new("/nonexistent-dial-user-dir"), None);
    let entries = listing["providers"].as_array().unwrap();
    assert!(!entries.is_empty());
    for entry in entries {
        assert!(entry["provider"].as_str().is_some());
        assert_eq!(entry["signed_in"], json!(false));
    }
}

#[test]
fn a_custom_endpoint_takes_a_key_by_its_saved_id_and_never_a_callers_url() {
    use crate::llm::model_config::{AuthMethod, ProviderSpec};
    let home = tempfile::tempdir().unwrap();
    let endpoint = |base_url: &str, auth| ProviderSpec {
        label: "Work proxy".into(),
        base_url: base_url.into(),
        auth,
        ..ProviderSpec::default()
    };
    let mut config = Config::default();
    config.providers.insert(
        "work".into(),
        endpoint("https://proxy.example/v1", AuthMethod::Auto),
    );
    config.providers.insert(
        "sso".into(),
        endpoint("https://sso.example/v1", AuthMethod::OAuth),
    );
    let saved = || Ok(config.clone());

    let found = key_endpoint("api-key:work", saved).unwrap();
    assert_eq!(
        (found.label.as_str(), found.base_url.as_str()),
        ("Work proxy", "https://proxy.example/v1")
    );
    for refused in [
        "api-key:sso",
        "api-key:missing",
        "api-key:https://proxy.example/v1",
        "work",
    ] {
        assert!(key_endpoint(refused, saved).is_err(), "{refused}");
    }
    // A catalog ID never reads the configuration.
    let catalog_id = catalog::key_providers().next().unwrap().id.clone();
    key_endpoint(&format!("api-key:{catalog_id}"), || {
        bail!("configuration read")
    })
    .unwrap();

    let entry = |listing: &Value| listing["endpoint_keys"].as_array().unwrap().clone();
    assert_eq!(
        entry(&providers(home.path(), Some(&config))),
        [
            json!({"provider":"api-key:work","name":"Work proxy","key_saved":false,"can_remove":true})
        ]
    );
    assert!(entry(&providers(home.path(), None)).is_empty());
    catalog::save_credential(home.path(), "https://proxy.example/v1", "sk-work").unwrap();
    assert_eq!(
        entry(&providers(home.path(), Some(&config)))[0]["key_saved"],
        true
    );
    let state = AuthState::default();
    let request = json!({"provider":"api-key:work","confirm":true});
    assert!(
        remove_api_key(
            &state,
            home.path(),
            saved,
            &json!({"provider":"api-key:work"})
        )
        .is_err()
    );
    assert_eq!(
        remove_api_key(&state, home.path(), saved, &request).unwrap(),
        json!({"provider":"api-key:work"})
    );
    assert_eq!(
        entry(&providers(home.path(), Some(&config)))[0]["key_saved"],
        false
    );
    assert!(
        !providers(home.path(), Some(&config))
            .to_string()
            .contains("sk-work")
    );
}

#[test]
fn login_frames_carry_the_url_and_prompt_id_and_the_callback_completes_the_sign_in() {
    let state = Arc::new(AuthState::default());
    let (frames_tx, frames_rx) = mpsc::channel::<Value>();
    let ui = RpcInteraction {
        provider: "copilot".into(),
        state: state.clone(),
        emit: move |frame| frames_tx.send(frame).is_ok(),
        gone: || false,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let worker = std::thread::spawn(move || {
        ui.auth(
            "https://github.com/login/device",
            Some("Enter code ABCD-1234"),
        );
        ui.progress("waiting for approval");
        ui.prompt("Paste the callback code", Some("code"), false)
    });
    let next = |label: &str| {
        frames_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("no {label} frame"))
    };
    let auth_url = next("auth_url");
    assert_eq!(auth_url["event"], "auth_url");
    assert_eq!(auth_url["url"], "https://github.com/login/device");
    assert_eq!(auth_url["instructions"], "Enter code ABCD-1234");
    let progress = next("auth_progress");
    assert_eq!(progress["event"], "auth_progress");
    let prompt = next("auth_prompt");
    assert_eq!(prompt["event"], "auth_prompt");
    assert_eq!(prompt["placeholder"], "code");
    assert_eq!(prompt["secret"], false);
    let prompt_id = prompt["prompt_id"].as_str().unwrap().to_owned();
    let reply = submit_code(
        &state,
        &json!({"prompt_id": prompt_id, "code": "callback-secret-42"}),
    )
    .unwrap();
    assert_eq!(reply, json!({"received": true}));
    assert_eq!(worker.join().unwrap().unwrap(), "callback-secret-42");
    assert!(frames_rx.try_iter().next().is_none());
    for frame in [&auth_url, &progress, &prompt] {
        assert!(!frame.to_string().contains("callback-secret-42"));
    }
}

#[test]
fn credential_prompts_are_marked_secret_and_the_answer_is_not_echoed() {
    let state = Arc::new(AuthState::default());
    let (frames_tx, frames_rx) = mpsc::channel::<Value>();
    let ui = RpcInteraction {
        provider: "xiaomi".into(),
        state: state.clone(),
        emit: move |frame| frames_tx.send(frame).is_ok(),
        gone: || false,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let worker = std::thread::spawn(move || ui.prompt_secret("Paste your API key", None, false));
    let prompt = frames_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(prompt["secret"], true);
    assert_eq!(prompt["provider"], "xiaomi");
    let prompt_id = prompt["prompt_id"].as_str().unwrap().to_owned();
    submit_code(&state, &json!({"prompt_id": prompt_id, "code": "sk-abc"})).unwrap();
    assert_eq!(worker.join().unwrap().unwrap(), "sk-abc");
    assert!(
        frames_rx
            .try_iter()
            .all(|frame| !frame.to_string().contains("sk-abc"))
    );
}

#[test]
fn mcp_logout_needs_a_server_and_reports_whether_a_grant_was_removed() {
    let state = AuthState::default();
    let home = tempfile::tempdir().unwrap();
    assert!(mcp_logout(&state, home.path(), &json!({})).is_err());
    assert_eq!(
        mcp_logout(&state, home.path(), &json!({"server":"docs"})).unwrap(),
        json!({"server":"docs","removed":false})
    );
}
