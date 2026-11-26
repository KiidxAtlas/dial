use super::*;
use crossterm::event::KeyEvent;

fn press(panel: &mut BackupOverlay, code: KeyCode) -> OverlayEvent {
    panel.handle(
        &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        &Keymap::defaults(),
    )
}

#[test]
fn draft_requires_explicit_review_and_cancel_never_emits_a_save() {
    let config = Config::default();
    let mut panel = BackupOverlay::new(&config, Scope::Project);
    panel.chain.set("openai/first, openai/second, openai/first");
    assert!(panel.changes(&toml::Table::new()).is_err());
    panel.row = 6;
    assert!(matches!(
        press(&mut panel, KeyCode::Enter),
        OverlayEvent::None
    ));
    assert!(panel.reviewing);
    assert_eq!(
        panel.candidate().unwrap().retry.fallback_chains["default"],
        ["openai/first", "openai/second", "openai/first"]
    );
    assert!(matches!(
        press(&mut panel, KeyCode::Esc),
        OverlayEvent::None
    ));
    assert!(panel.changes(&toml::Table::new()).is_err());
    assert!(matches!(
        press(&mut panel, KeyCode::Esc),
        OverlayEvent::Close
    ));
    assert!(config.retry.fallback_chains.is_empty());
}

/// Windows consoles (and terminals with keyboard enhancement) also report key releases; a
/// release must not move, toggle or save a second time.
#[test]
fn key_releases_never_repeat_the_press() {
    use crossterm::event::KeyEventKind;
    let release = |code| {
        Event::Key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ))
    };
    let keys = Keymap::defaults();
    let config = Config::default();
    let mut panel = BackupOverlay::new(&config, Scope::Project);
    press(&mut panel, KeyCode::Tab);
    assert_eq!(panel.row, 1);
    assert!(matches!(
        panel.handle(&release(KeyCode::Tab), &keys),
        OverlayEvent::None
    ));
    assert_eq!(panel.row, 1);
    panel.row = 5;
    press(&mut panel, KeyCode::Enter);
    let scope = panel.scope;
    panel.handle(&release(KeyCode::Enter), &keys);
    assert_eq!(panel.scope, scope);
    panel.reviewing = true;
    assert!(matches!(
        panel.handle(&release(KeyCode::Char('y')), &keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        panel.handle(&release(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(panel.reviewing);
}

#[test]
fn policy_scope_choices_and_review_preserve_order() {
    let config = Config::default();
    let mut panel = BackupOverlay::new(&config, Scope::Global);
    panel.chain.set("openai/second, openai/first");
    for row in [3, 4, 5] {
        panel.row = row;
        press(&mut panel, KeyCode::Right);
    }
    assert_eq!(panel.scope(), Scope::Project);
    assert_eq!(panel.on, FallbackOn::UsageLimit);
    assert_eq!(panel.revert, FallbackRevert::Never);
    panel.row = 6;
    press(&mut panel, KeyCode::Enter);
    assert_eq!(panel.changes(&toml::Table::new()).unwrap().len(), 3);
    assert!(
        matches!(press(&mut panel, KeyCode::Char('y')), OverlayEvent::Action(action) if action == "backup-models:save")
    );
    let candidate = panel.candidate().unwrap();
    let chain = fallback::fallback_models(&candidate, "default", &config.selection());
    assert_eq!(
        chain.iter().map(|s| s.model.as_str()).collect::<Vec<_>>(),
        ["second", "first"]
    );
}

#[test]
fn precedence_preview_is_the_runtime_resolver_and_stale_drafts_are_rejected() {
    let mut config = Config {
        provider: Some("openai".into()),
        model: "primary".into(),
        ..Config::default()
    };
    for (key, expected) in [
        ("openai/*", "wild"),
        ("openai/primary", "exact"),
        ("default", "role"),
    ] {
        config
            .retry
            .fallback_chains
            .insert(key.into(), vec![expected.into()]);
        assert_eq!(
            fallback::winning_chain_key(
                &config.retry.fallback_chains,
                "default",
                &config.selection()
            )
            .as_deref(),
            Some(key)
        );
        assert_eq!(
            fallback::fallback_models(&config, "default", &config.selection())[0].model,
            expected
        );
    }
    let panel = BackupOverlay::new(&config, Scope::Global);
    assert!(panel.check_unchanged(&config).is_ok());
    config.retry.fallback_revert = FallbackRevert::Never;
    assert!(panel.check_unchanged(&config).is_err());
}

#[test]
fn loading_an_existing_key_and_cached_role_choice_never_changes_configuration() {
    let mut config = Config::default();
    config
        .retry
        .fallback_chains
        .insert("task".into(), vec!["@small".into()]);
    let mut panel = BackupOverlay::new(&config, Scope::Project);
    panel.key.set("task");
    press(&mut panel, KeyCode::Enter);
    assert_eq!(panel.chain.text, "@small");
    panel.row = 2;
    press(&mut panel, KeyCode::Enter);
    assert_eq!(panel.selectors().len(), 2);
    assert_eq!(config.retry.fallback_chains["task"], ["@small"]);
    panel.chain.set("@unknown-role");
    assert!(panel.candidate().is_err());
}

#[test]
fn global_review_preserves_project_precedence_and_saves_only_its_layer() {
    let config = Config::default();
    let mut panel = BackupOverlay::new(&config, Scope::Global);
    panel.chain.set("openai/new-global");
    panel.set_project_layer(toml::from_str("[retry.fallback_chains]\ndefault=['openai/project']\n[retry]\nfallback_on='usage-limit'\n").unwrap());
    let candidate = panel.candidate().unwrap();
    assert_eq!(
        candidate.retry.fallback_chains["default"],
        ["openai/project"]
    );
    assert_eq!(candidate.retry.fallback_on, FallbackOn::UsageLimit);
    panel.reviewing = true;
    let layer = toml::from_str("[retry.fallback_chains]\ntask=['openai/task']\n").unwrap();
    let changes = panel.changes(&layer).unwrap();
    let Change::Set { value, .. } = &changes[0] else {
        panic!("expected set")
    };
    assert_eq!(value["task"][0].as_str(), Some("openai/task"));
    assert_eq!(value["default"][0].as_str(), Some("openai/new-global"));
}

#[test]
fn cached_choices_filter_and_ordered_list_reorders_without_text_editing() {
    let mut panel = BackupOverlay::new(&Config::default(), Scope::Project);
    panel.choices = vec!["openai/first".into(), "anthropic/second".into()];
    panel.row = 2;
    for character in "anthropic".chars() {
        press(&mut panel, KeyCode::Char(character));
    }
    press(&mut panel, KeyCode::Enter);
    assert_eq!(panel.selectors(), ["anthropic/second"]);
    panel.chain.set("openai/first, anthropic/second");
    panel.row = 1;
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
        &Keymap::defaults(),
    );
    assert_eq!(panel.selectors(), ["anthropic/second", "openai/first"]);
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT)),
        &Keymap::defaults(),
    );
    assert_eq!(panel.selectors(), ["openai/first", "anthropic/second"]);
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL)),
        &Keymap::defaults(),
    );
    assert_eq!(panel.selectors(), ["anthropic/second"]);
}
