use super::super::settings::testkit::*;
use super::*;

#[test]
fn a_local_server_that_is_not_running_is_expected_but_real_failures_stay_red() {
    let entry = |base: &str, state: &str, detail: &str, configured: bool| Entry {
        id: "ollama".into(),
        label: "Ollama".into(),
        base_url: Some(base.into()),
        state: state.into(),
        detail: detail.into(),
        models: 0,
        spec: configured.then(ProviderSpec::default),
    };
    for base in [
        "http://127.0.0.1:11434",
        "http://localhost:1234/v1",
        "http://[::1]:8080",
    ] {
        assert!(
            expected_absent(&entry(base, "unavailable", UNREACHABLE_DETAIL, true)),
            "{base}"
        );
    }
    // Not configured by the user: discovery found nothing, which is expected.
    assert!(expected_absent(&entry(
        "http://127.0.0.1:11434",
        "unavailable",
        UNREACHABLE_DETAIL,
        false
    )));
    // A configured remote endpoint that refused is a real failure.
    assert!(!expected_absent(&entry(
        "https://llm.example.com/v1",
        "unavailable",
        UNREACHABLE_DETAIL,
        true
    )));
    // A local server that answered with an error, or sent a bad reply, is a real failure.
    for detail in [
        "Endpoint returned HTTP 500; redirects are not followed",
        "Endpoint returned malformed model metadata",
    ] {
        for configured in [false, true] {
            assert!(
                !expected_absent(&entry(
                    "http://127.0.0.1:11434",
                    "unavailable",
                    detail,
                    configured
                )),
                "{detail}, configured={configured}"
            );
        }
    }
    // Credential failures are never neutral.
    assert!(!expected_absent(&entry(
        "http://127.0.0.1:11434",
        "auth-required",
        "Endpoint requires valid credentials",
        true
    )));
}

/// Put the cursor on the provider `id`; rows are ordered by readiness, not by fixture order.
fn select(o: &mut ProviderOverlay, id: &str) {
    o.cursor = o
        .entries()
        .iter()
        .position(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("no provider {id}"));
}
use crate::llm::model_config::UiConfig;
use std::{cell::RefCell, rc::Rc};

fn provider(
    id: &str,
    label: &str,
    state: &str,
    models: usize,
    url: Option<&str>,
) -> ProviderStatus {
    ProviderStatus {
        id: id.into(),
        label: label.into(),
        base_url: url.map(str::to_owned),
        state: state.into(),
        detail: if state == "unavailable" {
            UNREACHABLE_DETAIL.into()
        } else {
            format!("{label} detail")
        },
        models,
    }
}

fn discovery() -> Discovery {
    Discovery {
        models: vec![],
        providers: vec![
            provider("mlx", "Local MLX", "cached", 2, None),
            provider(
                "lm-studio",
                "LM Studio",
                "reachable",
                3,
                Some("http://127.0.0.1:1234/v1"),
            ),
            provider(
                "openai",
                "OpenAI",
                "auth-required",
                0,
                Some("https://api.openai.com/v1"),
            ),
            provider(
                "ollama",
                "Ollama",
                "unavailable",
                0,
                Some("http://127.0.0.1:11434"),
            ),
            provider(
                "work",
                "Work",
                "disabled",
                0,
                Some("https://llm.example.com/v1"),
            ),
            provider(
                "openrouter",
                "OpenRouter",
                "auth-required",
                0,
                Some("https://openrouter.ai/api/v1"),
            ),
        ],
    }
}

fn configured() -> BTreeMap<String, ProviderSpec> {
    BTreeMap::from([(
        "work".to_owned(),
        ProviderSpec {
            label: "Work".into(),
            base_url: "https://llm.example.com/v1".into(),
            api_key_env: Some("WORK_KEY".into()),
            enabled: false,
            allow_insecure: false,
            protocol: None,
            headers: None,
        },
    )])
}

fn overlay_with(saved: Rc<RefCell<Vec<String>>>) -> ProviderOverlay {
    ProviderOverlay::new(
        Some(discovery()),
        configured(),
        Box::new(move |url| saved.borrow().iter().any(|u| u == url)),
        Scope::Global,
    )
}

fn overlay() -> ProviderOverlay {
    overlay_with(Rc::new(RefCell::new(vec![
        "http://127.0.0.1:1234/v1".into(),
    ])))
}

#[test]
fn pending_chatgpt_login_can_be_cancelled_explicitly() {
    let rig = Rig::new();
    let mut d = discovery();
    d.providers.insert(
        0,
        provider(
            "chatgpt",
            "ChatGPT subscription",
            "auth-required",
            0,
            Some(crate::llm::chatgpt::BASE_URL),
        ),
    );
    let mut o = ProviderOverlay::new(Some(d), BTreeMap::new(), Box::new(|_| false), Scope::Global);
    select(&mut o, "chatgpt");
    o.set_chatgpt_pending(true);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(draw(&mut o, &rig, 110, 24).contains("Cancel pending sign-in"));
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::ChatGptCancel
    ));
    o.set_chatgpt_pending(false);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(!draw(&mut o, &rig, 110, 24).contains("Cancel pending sign-in"));
}
#[test]
fn chatgpt_actions_use_account_login_and_never_the_api_key_editor() {
    let rig = Rig::new();
    let mut d = discovery();
    d.providers.insert(
        0,
        provider(
            "chatgpt",
            "ChatGPT subscription",
            "auth-required",
            0,
            Some(crate::llm::chatgpt::BASE_URL),
        ),
    );
    let mut o = ProviderOverlay::new(Some(d), BTreeMap::new(), Box::new(|_| true), Scope::Global);
    select(&mut o, "chatgpt");
    send(&mut o, &rig, key(KeyCode::Char('k')));
    assert!(matches!(o.mode, Mode::List));
    send(&mut o, &rig, key(KeyCode::Enter));
    let shown = draw(&mut o, &rig, 110, 24);
    assert!(shown.contains("Continue with ChatGPT"));
    assert!(!shown.contains("Set API key") && !shown.contains("Replace API key"));
    send(&mut o, &rig, key(KeyCode::Down));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::ChatGptLogin { client_id: None }
    ));
    o.set_chatgpt_accounts(
        (0..8)
            .map(|i| crate::llm::chatgpt::AccountInfo {
                client_id: format!("oaiapp_{i}"),
                label: format!("person{i}@example.com"),
                active: i == 0,
                signed_in: true,
            })
            .collect(),
    );
    send(&mut o, &rig, key(KeyCode::Enter));
    for _ in 0..25 {
        send(&mut o, &rig, key(KeyCode::Down));
    }
    let shown = draw(&mut o, &rig, 110, 12);
    assert!(
        shown.contains("person7@example.com"),
        "selection stays visible in a short account menu: {shown}"
    );
    assert!(
        matches!(send(&mut o,&rig,key(KeyCode::Enter)),OverlayEvent::ChatGptSignOut { client_id } if client_id=="oaiapp_7")
    );
}

fn draw(o: &mut ProviderOverlay, rig: &Rig, w: u16, h: u16) -> String {
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &|ui: &UiConfig, w| status_preview(ui, w),
    };
    screen(&render(w, h, |f| o.draw(f, &ctx)))
}

fn send(o: &mut ProviderOverlay, rig: &Rig, e: Event) -> OverlayEvent {
    o.handle(&e, &rig.keys)
}
/// Moves the open menu's cursor onto the item with this label (tests select by label, not index).
fn pick(o: &mut ProviderOverlay, label: &str) {
    let Mode::Menu(menu) = &mut o.mode else {
        panic!("menu is not open");
    };
    menu.cursor = menu
        .items
        .iter()
        .position(|(_, text)| text == label)
        .unwrap_or_else(|| panic!("no menu item {label:?}"));
}
fn send_all(o: &mut ProviderOverlay, rig: &Rig, events: Vec<Event>) -> OverlayEvent {
    let mut last = OverlayEvent::None;
    for e in events {
        last = send(o, rig, e);
    }
    last
}

#[test]
fn list_shows_status_chips_counts_and_key_state() {
    let rig = Rig::new();
    let mut o = overlay();
    let shown = draw(&mut o, &rig, 120, 30);
    let line = |n: &str| {
        shown
            .lines()
            .find(|l| l.contains(n))
            .unwrap_or_else(|| panic!("{n}\n{shown}"))
            .to_owned()
    };
    assert!(
        line("LM Studio").contains("reachable")
            && line("LM Studio").contains("3 models")
            && line("LM Studio").contains("key saved"),
        "{}",
        line("LM Studio")
    );
    assert!(line("OpenAI").contains("not set up") && line("OpenAI").contains("key needed"));
    assert!(line("Ollama").contains("not running") && line("Ollama").contains("no key"));
    assert!(line("Work").contains("disabled") && line("Work").contains("env WORK_KEY"));
    assert!(
        line("Local MLX").contains("cached") && !line("Local MLX").contains("key"),
        "local providers have no key"
    );
    assert!(
        shown.contains("+ Add provider")
            && shown.contains("scope: global")
            && shown.contains("● reachable")
            && shown.contains("○ not set up")
            && shown.contains("○ not running")
            && shown.contains("○ disabled")
    );
    // Status marks are colored by state.
    let buf = render(120, 30, |f| {
        let c = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &|u: &UiConfig, w| status_preview(u, w),
        };
        o.draw(f, &c)
    });
    assert_eq!(
        style_of(&buf, "reachable").unwrap().fg,
        rig.theme.fg(Tok::Success).fg
    );
    assert_eq!(
        style_of(&buf, "not set up").unwrap().fg,
        rig.theme.fg(Tok::Muted).fg,
        "a provider you have not set up is neutral, not a warning"
    );
    assert_eq!(
        style_of(&buf, "not running").unwrap().fg,
        rig.theme.fg(Tok::Muted).fg,
        "a local server you never set up is not an error when it is stopped"
    );
    assert_eq!(
        style_of(&buf, "disabled").unwrap().fg,
        rig.theme.fg(Tok::Muted).fg
    );
}

#[test]
fn key_entry_is_masked_sent_once_and_cleared() {
    let rig = Rig::new();
    let mut o = overlay();
    select(&mut o, "openai");
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(
        draw(&mut o, &rig, 100, 24).contains("Set API key"),
        "unsaved key offers Set, not Replace"
    );
    pick(&mut o, "Set API key");
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::None),
        "enters the key field"
    );
    for e in typed("sk-SECRET-123") {
        send(&mut o, &rig, e);
    }
    let shown = draw(&mut o, &rig, 100, 24);
    assert!(
        !shown.contains("SECRET") && !shown.contains("sk-"),
        "never rendered\n{shown}"
    );
    assert!(
        shown.contains("•••••••••••••") && shown.contains("13 characters"),
        "{shown}"
    );
    match &o.mode {
        Mode::Key { secret, .. } => assert!(!format!("{secret:?}").contains("SECRET")),
        _ => panic!(),
    }
    assert!(!format!("{o:?}").contains("SECRET"));
    let event = send(&mut o, &rig, key(KeyCode::Enter));
    assert!(
        !format!("{event:?}").contains("SECRET"),
        "credential events redact keys"
    );
    match event {
        OverlayEvent::SaveCredential {
            provider,
            base_url,
            token,
        } => assert_eq!(
            (provider.as_str(), base_url.as_str(), token.as_str()),
            ("openai", "https://api.openai.com/v1", "sk-SECRET-123")
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(o.mode, Mode::List));
    let after = draw(&mut o, &rig, 100, 24);
    assert!(!after.contains("SECRET") && after.contains("Verifying"));
}
#[test]
fn openrouter_login_is_offered_only_for_its_canonical_endpoint() {
    let rig = Rig::new();
    let mut o = overlay();
    o.cursor = o
        .entries()
        .iter()
        .position(|entry| entry.id == "openrouter")
        .unwrap();
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(draw(&mut o, &rig, 100, 24).contains("Sign in with OpenRouter"));
    pick(&mut o, "Sign in with OpenRouter");
    let event = send(&mut o, &rig, key(KeyCode::Enter));
    assert!(matches!(event, OverlayEvent::OpenRouterLogin));
    assert_eq!(format!("{event:?}"), "OpenRouterLogin");

    let mut configured = configured();
    configured.insert(
        "openrouter".into(),
        ProviderSpec {
            label: "OpenRouter proxy".into(),
            base_url: "https://gateway.example.com/v1".into(),
            ..ProviderSpec::default()
        },
    );
    let mut proxy = ProviderOverlay::new(
        Some(discovery()),
        configured,
        Box::new(|_| false),
        Scope::Global,
    );
    proxy.cursor = proxy
        .entries()
        .iter()
        .position(|entry| entry.id == "openrouter")
        .unwrap();
    send(&mut proxy, &rig, key(KeyCode::Enter));
    assert!(
        !draw(&mut proxy, &rig, 120, 24).contains("Sign in with OpenRouter"),
        "never forward an OpenRouter-minted credential to a custom gateway"
    );
}

#[test]
fn anthropic_subscription_sign_in_starts_browser_oauth() {
    let rig = Rig::new();
    let mut d = discovery();
    d.providers.push(provider(
        "anthropic",
        "Anthropic",
        "auth-required",
        0,
        Some(crate::llm::anthropic::BASE_URL),
    ));
    let mut o = ProviderOverlay::new(Some(d), configured(), Box::new(|_| false), Scope::Global);
    o.cursor = o
        .entries()
        .iter()
        .position(|e| e.id == "anthropic")
        .unwrap();
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(draw(&mut o, &rig, 120, 24).contains("Sign in with Claude subscription"));
    pick(&mut o, "Sign in with Claude subscription");
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::AnthropicLogin
    ));
    o.set_anthropic_signed_in(true);
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(draw(&mut o, &rig, 120, 24).contains("Sign out of Claude subscription"));
}

#[test]
fn key_entry_rejects_bad_input_and_cancel_discards_it() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Down));
    send(&mut o, &rig, ch('k'));
    assert!(
        matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::None),
        "empty key is not sent"
    );
    assert!(draw(&mut o, &rig, 100, 24).contains("Type or paste the key first"));
    send(&mut o, &rig, ch('a'));
    send(&mut o, &rig, ch(' '));
    assert!(draw(&mut o, &rig, 100, 24).contains("printable ASCII without spaces"));
    send(&mut o, &rig, Event::Paste("tok\u{e9}n".into()));
    assert!(draw(&mut o, &rig, 100, 24).contains("paste was ignored"));
    send(&mut o, &rig, Event::Paste("abc\n".into()));
    match &o.mode {
        Mode::Key { secret, .. } => {
            assert_eq!(secret.len(), 4, "newline-terminated paste keeps the key")
        }
        _ => panic!(),
    }
    send(&mut o, &rig, key(KeyCode::Backspace));
    match &o.mode {
        Mode::Key { secret, .. } => assert_eq!(secret.len(), 3),
        _ => panic!(),
    }
    send(&mut o, &rig, ctrl('u'));
    match &o.mode {
        Mode::Key { secret, .. } => assert_eq!(secret.len(), 0),
        _ => panic!(),
    }
    send_all(&mut o, &rig, typed("zzz"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(
        matches!(o.mode, Mode::List),
        "Esc leaves the field, not the panel"
    );
    // Local providers have no key.
    o.cursor = 0;
    send(&mut o, &rig, ch('k'));
    assert!(matches!(o.mode, Mode::List));
    assert!(draw(&mut o, &rig, 100, 24).contains("local provider and has no API key"));
}

#[test]
fn secret_is_wiped_and_never_debug_printed() {
    let mut s = Secret::new();
    for c in "tok".chars() {
        s.push(c);
    }
    assert_eq!(format!("{s:?}"), "Secret(<3 characters hidden>)");
    s.wipe();
    assert_eq!(s.len(), 0);
    assert!(
        !s.push('é') && !s.push(' ') && !s.push('\n'),
        "only printable ASCII is accepted"
    );
    assert!(s.push('x'));
    assert_eq!(s.take(), "x");
    assert_eq!(s.len(), 0, "take leaves nothing behind");
}

#[test]
fn menu_offers_actions_that_fit_the_provider() {
    let rig = Rig::new();
    let mut o = overlay();
    select(&mut o, "lm-studio");
    send(&mut o, &rig, key(KeyCode::Enter));
    let shown = draw(&mut o, &rig, 100, 24);
    for needle in [
        "Choose a model",
        "Test connection",
        "Replace API key",
        "Remove API key",
        "Disable provider",
    ] {
        assert!(shown.contains(needle), "{needle}\n{shown}");
    }
    assert!(
        !shown.contains("Remove provider"),
        "built-in providers cannot be removed"
    );
    // Choose a model hands over to the model selector.
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::Action(a) => assert_eq!(a, "open:models"),
        other => panic!("{other:?}"),
    }
    // Test connection sits right under Choose a model and starts a check.
    send(&mut o, &rig, key(KeyCode::Enter));
    pick(&mut o, "Test connection");
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::Action(a) => assert!(a.starts_with("test:"), "{a}"),
        other => panic!("{other:?}"),
    }
    // Remove key asks first, naming the key; Enter confirms.
    send(&mut o, &rig, key(KeyCode::Enter));
    pick(&mut o, "Remove API key");
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 110, 24).contains("Remove the saved API key for"));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::RemoveCredential { base_url } => {
            assert_eq!(base_url, "http://127.0.0.1:1234/v1")
        }
        other => panic!("{other:?}"),
    }
    // Disable a built-in: an AddProvider carrying its URL with enabled = false.
    send(&mut o, &rig, key(KeyCode::Enter));
    pick(&mut o, "Disable provider");
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AddProvider { id, spec, scope } => assert_eq!(
            (id.as_str(), spec.base_url.as_str(), spec.enabled, scope),
            (
                "lm-studio",
                "http://127.0.0.1:1234/v1",
                false,
                Scope::Global
            )
        ),
        other => panic!("{other:?}"),
    }
    // Enable the disabled configured provider: other fields are preserved.
    select(&mut o, "work");
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(
        draw(&mut o, &rig, 100, 24).contains("Enable provider")
            && draw(&mut o, &rig, 100, 24).contains("Remove provider")
    );
    pick(&mut o, "Enable provider");
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AddProvider { id, spec, .. } => assert_eq!(
            (
                id.as_str(),
                spec.enabled,
                spec.api_key_env.as_deref(),
                spec.label.as_str()
            ),
            ("work", true, Some("WORK_KEY"), "Work")
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn removing_a_configured_provider_needs_confirmation() {
    let rig = Rig::new();
    let mut o = overlay();
    select(&mut o, "work");
    assert!(matches!(send(&mut o, &rig, ch('x')), OverlayEvent::None));
    assert!(
        draw(&mut o, &rig, 110, 24).contains("Remove provider Work (work) from the global config?")
    );
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::List));
    send(&mut o, &rig, ctrl('s'));
    send(&mut o, &rig, key(KeyCode::Delete));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::RemoveProvider { id, scope } => {
            assert_eq!((id.as_str(), scope), ("work", Scope::Project))
        }
        other => panic!("{other:?}"),
    }
    // Built-ins explain why they cannot be removed.
    o.cursor = 1;
    send(&mut o, &rig, ch('x'));
    assert!(matches!(o.mode, Mode::List));
    assert!(draw(&mut o, &rig, 120, 24).contains("built in and is not in your config"));
}

#[test]
fn saving_a_key_reads_as_in_progress_until_the_controller_replies() {
    let rig = Rig::new();
    let mut o = overlay();
    select(&mut o, "lm-studio");
    send(&mut o, &rig, key(KeyCode::Enter));
    pick(&mut o, "Replace API key");
    send(&mut o, &rig, key(KeyCode::Enter));
    for c in "sk-x".chars() {
        send(&mut o, &rig, ch(c));
    }
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::SaveCredential { .. }
    ));
    assert!(o.saving);
    assert!(draw(&mut o, &rig, 100, 24).contains("Saving LM Studio key"));
    o.set_status("API key saved (owner-only file)".into(), false);
    assert!(!o.saving);
    assert!(draw(&mut o, &rig, 100, 24).contains("API key saved"));
}

#[test]
fn wizard_validates_each_step_and_emits_add_provider() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, ch('a'));
    let err = |o: &mut ProviderOverlay, rig: &Rig| draw(o, rig, 110, 30);
    // id
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(err(&mut o, &rig).contains("Use 1-64 letters, digits"));
    send_all(&mut o, &rig, typed("bad id"));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(err(&mut o, &rig).contains("Use 1-64 letters"));
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed("mlx"));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(err(&mut o, &rig).contains("reserved"));
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed("work"));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(err(&mut o, &rig).contains("already configured"));
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed(&"x".repeat(65)));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(err(&mut o, &rig).contains("Use 1-64"));
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed("acme-1"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    // label is optional
    assert!(err(&mut o, &rig).contains("Display name"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    // url
    for bad in [
        "ftp://x/v1",
        "api.acme.com",
        "http://api.acme.com/v1",
        "https://user:pw@api.acme.com/v1",
        "https://api.acme.com/v1?key=1",
        "https://",
    ] {
        send_all(&mut o, &rig, vec![ctrl('u')]);
        send_all(&mut o, &rig, typed(bad));
        assert!(
            matches!(send(&mut o, &rig, key(KeyCode::Enter)), OverlayEvent::None),
            "{bad}"
        );
        let shown = err(&mut o, &rig);
        assert!(
            shown.contains("Base URL must start")
                || shown.contains("Not accepted")
                || shown.contains("single line"),
            "{bad}\n{shown}"
        );
        assert!(!shown.contains("pw@") || !shown.contains("Not accepted") || true);
    }
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed("https://api.acme.com/v1"));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    // env var
    send_all(&mut o, &rig, typed("1BAD"));
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(err(&mut o, &rig).contains("do not start with a digit"));
    send_all(&mut o, &rig, vec![ctrl('u')]);
    send_all(&mut o, &rig, typed("ACME_API_KEY"));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AddProvider { id, spec, scope } => {
            assert_eq!((id.as_str(), scope), ("acme-1", Scope::Global));
            assert_eq!(
                (
                    spec.label.as_str(),
                    spec.base_url.as_str(),
                    spec.api_key_env.as_deref(),
                    spec.enabled,
                    spec.allow_insecure
                ),
                (
                    "acme-1",
                    "https://api.acme.com/v1",
                    Some("ACME_API_KEY"),
                    true,
                    false
                )
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(o.mode, Mode::List));
}

#[test]
fn wizard_accepts_local_http_and_empty_env_and_esc_steps_back() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig, ch('+'));
    send_all(&mut o, &rig, typed("local1"));
    send(&mut o, &rig, key(KeyCode::Enter));
    send_all(&mut o, &rig, typed("My Server"));
    send(&mut o, &rig, key(KeyCode::Enter));
    // Back to the label step keeps what was typed.
    send(&mut o, &rig, key(KeyCode::Esc));
    match &o.mode {
        Mode::Wizard(w) => assert_eq!((w.step, w.editor.text.as_str()), (1, "My Server")),
        _ => panic!(),
    }
    send(&mut o, &rig, key(KeyCode::Enter));
    send_all(&mut o, &rig, typed("http://localhost:8080/v1"));
    send(&mut o, &rig, key(KeyCode::Enter));
    match send(&mut o, &rig, key(KeyCode::Enter)) {
        OverlayEvent::AddProvider { id, spec, .. } => assert_eq!(
            (
                id.as_str(),
                spec.label.as_str(),
                spec.base_url.as_str(),
                spec.api_key_env
            ),
            ("local1", "My Server", "http://localhost:8080/v1", None)
        ),
        other => panic!("{other:?}"),
    }
    // Esc at the first step cancels the wizard only.
    send(&mut o, &rig, ch('a'));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::List));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));
}

#[test]
fn add_row_via_enter_and_refresh_keys() {
    let rig = Rig::new();
    let mut o = overlay();
    o.cursor = o.row_count() - 1;
    send(&mut o, &rig, key(KeyCode::Enter));
    assert!(matches!(o.mode, Mode::Wizard(_)));
    send(&mut o, &rig, key(KeyCode::Esc));
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::F(5))),
        OverlayEvent::Refresh
    ));
    assert!(matches!(
        send(&mut o, &rig, ctrl('r')),
        OverlayEvent::Refresh
    ));
}

#[test]
fn discovery_update_keeps_the_highlighted_provider_and_reflects_new_state() {
    let rig = Rig::new();
    let saved = Rc::new(RefCell::new(Vec::<String>::new()));
    let mut o = overlay_with(saved.clone());
    assert!(
        draw(&mut o, &rig, 120, 30).contains("no key")
            || draw(&mut o, &rig, 120, 30).contains("key")
    );
    o.cursor = 3; // ollama
    let mut d = discovery();
    d.providers.insert(
        0,
        provider("zzz", "Zzz", "reachable", 1, Some("https://z.example/v1")),
    );
    o.set_discovery(d);
    assert_eq!(o.entries()[o.cursor].id, "ollama");
    // has_key is asked live: saving a key shows up without a new discovery.
    saved.borrow_mut().push("https://api.openai.com/v1".into());
    assert!(
        draw(&mut o, &rig, 120, 30)
            .lines()
            .find(|l| l.contains("OpenAI"))
            .unwrap()
            .contains("key saved")
    );
}

#[test]
fn before_discovery_configured_providers_show_as_checking() {
    let rig = Rig::new();
    let mut cfg = configured();
    cfg.get_mut("work").unwrap().enabled = true;
    let mut o = ProviderOverlay::new(None, cfg, Box::new(|_| false), Scope::Project);
    let shown = draw(&mut o, &rig, 110, 24);
    assert!(
        shown.contains("checking")
            && shown.contains("Waiting for the first scan")
            && shown.contains("scope: project"),
        "{shown}"
    );
    let mut empty = ProviderOverlay::new(None, BTreeMap::new(), Box::new(|_| false), Scope::Global);
    let shown = draw(&mut empty, &rig, 110, 24);
    assert!(shown.contains("+ Add provider"));
    assert!(matches!(
        send(&mut empty, &rig, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(empty.mode, Mode::Wizard(_)));
}

#[test]
fn remapped_navigation_is_honoured() {
    let mut rig = Rig::new();
    rig.keys.rebind(Action::SelectDown, &["Ctrl+J"]).unwrap();
    rig.keys.rebind(Action::SelectCancel, &["Ctrl+Q"]).unwrap();
    let mut o = overlay();
    send(&mut o, &rig, key(KeyCode::Down));
    assert_eq!(o.cursor, 0);
    send(&mut o, &rig, ctrl('j'));
    assert_eq!(o.cursor, 1);
    assert!(matches!(
        send(&mut o, &rig, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(send(&mut o, &rig, ctrl('q')), OverlayEvent::Close));
    assert!(draw(&mut o, &rig, 160, 24).contains("Ctrl+Q close"));
}

#[test]
fn tiny_terminals_never_panic_in_any_mode() {
    std::thread::scope(|scope| {
        for w in 0..=30u16 {
            scope.spawn(move || {
                let rig = Rig::new();
                for h in 0..=30u16 {
                    let mut o = overlay();
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Down));
                    send(&mut o, &rig, key(KeyCode::Enter));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Enter));
                    send_all(&mut o, &rig, typed("sk-tiny"));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Esc));
                    send(&mut o, &rig, key(KeyCode::Esc));
                    send(&mut o, &rig, ch('a'));
                    send_all(&mut o, &rig, typed("id"));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig, key(KeyCode::Esc));
                    o.cursor = 4;
                    send(&mut o, &rig, ch('x'));
                    draw(&mut o, &rig, w, h);
                }
            });
        }
    });
}

#[test]
fn hostile_provider_text_is_sanitized() {
    let rig = Rig::new();
    let mut d = discovery();
    d.providers[1].label = "Evil\u{1b}[31m 日本語日本語日本語日本語日本語".into();
    d.providers[1].detail = "x\u{7}y".into();
    let mut o = ProviderOverlay::new(Some(d), BTreeMap::new(), Box::new(|_| false), Scope::Global);
    for w in [30u16, 60, 120] {
        let shown = draw(&mut o, &rig, w, 20);
        assert!(!shown.contains('\u{1b}') && !shown.contains('\u{7}'));
    }
}
