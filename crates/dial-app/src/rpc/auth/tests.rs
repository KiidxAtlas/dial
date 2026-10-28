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
    let listing = providers(Path::new("/nonexistent-dial-user-dir"));
    let entries = listing["providers"].as_array().unwrap();
    assert!(!entries.is_empty());
    for entry in entries {
        assert!(entry["provider"].as_str().is_some());
        assert_eq!(entry["signed_in"], json!(false));
    }
}

#[test]
fn login_frames_carry_the_url_and_prompt_id_and_the_callback_completes_the_sign_in() {
    let state = Arc::new(AuthState::default());
    let (frames_tx, frames_rx) = mpsc::channel::<Value>();
    let ui = RpcInteraction {
        provider: "copilot",
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
