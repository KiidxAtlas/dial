use super::*;

#[test]
fn onboarding_has_explicit_hosted_and_local_paths_and_escape_is_cancel() {
    assert!(matches!(
        onboarding_destination(Some(1)),
        Some(Which::Providers)
    ));
    assert!(matches!(
        onboarding_destination(Some(0)),
        Some(Which::Models)
    ));
    assert_eq!(ONBOARDING_CHOICES[0].0, "Local model");
    assert_eq!(ONBOARDING_CHOICES.last().unwrap().0, "Quit setup");
    for cancel in [None, Some(2), Some(99)] {
        assert!(onboarding_destination(cancel).is_none(), "{cancel:?}");
    }
    // Cancelling a panel returns a Cancel flow; it is not an implicit local selection.
    assert!(matches!(
        onboarding_flow(&OverlayOutcome::default()),
        OnboardingFlow::Cancel
    ));
}

#[test]
fn onboarding_names_running_local_servers_and_highlights_local() {
    let none = LocalHint::default();
    assert_eq!(
        onboarding_default(&none),
        1,
        "no local path highlights Hosted, not Quit"
    );
    assert!(!onboarding_prompt(&none).contains("Running on this machine"));

    let running = LocalHint {
        running: vec!["Ollama", "LM Studio"],
        suggestion: None,
    };
    assert_eq!(onboarding_default(&running), 0);
    assert!(matches!(
        onboarding_destination(Some(onboarding_default(&running))),
        Some(Which::Models)
    ));
    let prompt = onboarding_prompt(&running);
    assert!(
        prompt.starts_with("Running on this machine: Ollama, LM Studio."),
        "{prompt}"
    );
    assert!(prompt.contains("Esc cancels"), "{prompt}");
}

#[test]
fn onboarding_suggests_a_fitting_model_when_no_server_runs() {
    let hint = LocalHint {
        running: Vec::new(),
        suggestion: Some("mlx-community/Qwen2.5-7B-Instruct-4bit"),
    };
    assert_eq!(onboarding_default(&hint), 0);
    let prompt = onboarding_prompt(&hint);
    assert!(
        prompt.contains("enough memory for mlx-community/Qwen2.5-7B-Instruct-4bit"),
        "{prompt}"
    );
    assert!(prompt.contains("only after you confirm"), "{prompt}");
}

#[test]
fn onboarding_verification_is_an_explicit_action_even_after_a_selection_is_saved() {
    let outcome = OverlayOutcome {
        next: Some("command:setup choose verify-model".into()),
        latest: Some(Config::default()),
        ..Default::default()
    };
    assert!(matches!(
        onboarding_flow(&outcome),
        OnboardingFlow::VerifySelectedModel
    ));
}

#[test]
fn first_run_saves_edit_freely_only_when_approval_was_never_chosen() {
    use crate::config::types::Approval;
    let (_home, _project, layout, config) = crate::app::tests::config();
    assert_eq!(
        config.approval,
        Approval::Auto,
        "built-in default is unchanged"
    );
    let mut app = App::new(
        layout.clone(),
        config,
        toml::Table::new(),
        Default::default(),
        Vec::new(),
        Default::default(),
    )
    .unwrap();
    let notice = adopt_first_run_approval(&mut app).expect("first run explains the mode");
    assert!(notice.starts_with("Edit freely"), "{notice}");
    assert_eq!(app.config.approval, Approval::Write);
    let reloaded = Config::load(&layout, None).unwrap();
    assert_eq!(
        reloaded.approval,
        Approval::Write,
        "saved as the user's choice"
    );
    assert_eq!(reloaded.setting_sources.get("approval"), Some(&"global"));

    // A later run, or a user who already chose, is never touched.
    let mut later = App::new(
        layout.clone(),
        reloaded,
        toml::Table::new(),
        Default::default(),
        Vec::new(),
        Default::default(),
    )
    .unwrap();
    later.config.approval = Approval::Ask;
    assert!(adopt_first_run_approval(&mut later).is_none());
    assert_eq!(later.config.approval, Approval::Ask);
}
