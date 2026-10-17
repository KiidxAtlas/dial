use super::testkit::*;
use super::*;
use std::cell::RefCell;
#[test]
fn coherent_runtime_learning_preferences_have_a_separate_view() {
    let mut coding = overlay().coding_only();
    assert!(!coding.tabs.iter().any(|(tab, _)| *tab == "Improvement"));
    for query in [
        "laya",
        "improve.enabled",
        "training.rank",
        "memory.autolearn",
    ] {
        coding.search = query.into();
        assert!(coding.visible().iter().all(|row| !matches!(row, Row::Setting(i) if SettingsOverlay::learning_setting(&crate::config::schema::registry()[*i]))));
    }
    coding.search = "teach this model".into();
    assert!(coding.visible().iter().any(|row| matches!(row, Row::Setting(i) if crate::config::schema::registry()[*i].path == "improve.teach")));
    let mut learning = overlay().learning_only();
    learning.search = "laya".into();
    assert!(learning.visible().iter().any(|row| matches!(row, Row::Setting(i) if crate::config::schema::registry()[*i].path == "laya.enabled")));
    learning.search = "show_tools".into();
    assert!(
        !learning
            .visible()
            .iter()
            .any(|row| matches!(row, Row::Setting(_)))
    );
}

fn table(src: &str) -> Table {
    toml::from_str(src).unwrap()
}

fn overlay() -> SettingsOverlay {
    SettingsOverlay::new(
        Table::new(),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec!["dark".into(), "light".into(), "mono".into()],
    )
}

fn send(o: &mut SettingsOverlay, keys: &Keymap, event: Event) -> OverlayEvent {
    o.handle(&event, keys)
}

fn send_all(o: &mut SettingsOverlay, keys: &Keymap, events: Vec<Event>) -> OverlayEvent {
    let mut last = OverlayEvent::None;
    for e in events {
        last = o.handle(&e, keys);
    }
    last
}

fn draw(o: &mut SettingsOverlay, rig: &Rig, w: u16, h: u16) -> String {
    let preview = |ui: &UiConfig, width: u16| status_preview(ui, width);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    screen(&render(w, h, |f| o.draw(f, &ctx)))
}

fn goto(o: &mut SettingsOverlay, keys: &Keymap, path: &str) {
    o.search.clear();
    // Advanced settings sit behind their section's "Show advanced" row; open it so the row exists.
    let def = crate::config::schema::registry()
        .iter()
        .find(|d| d.path == path)
        .unwrap();
    if crate::config::schema::is_advanced(path) {
        o.advanced_open.insert((def.tab, def.section));
    }
    o.tab = o.tabs.iter().position(|(t, _)| *t == def.tab).unwrap();
    let at = o
        .visible()
        .iter()
        .position(
            |r| matches!(r, Row::Setting(i) if crate::config::schema::registry()[*i].path == path),
        )
        .unwrap();
    o.cursor = at;
    let _ = keys;
}

fn path_at_cursor(o: &SettingsOverlay) -> &'static str {
    match o.current_row() {
        Some(Row::Setting(i)) => crate::config::schema::registry()[i].path,
        _ => "",
    }
}

fn shows(o: &SettingsOverlay, path: &str) -> bool {
    o.visible()
        .iter()
        .any(|r| matches!(r, Row::Setting(i) if crate::config::schema::registry()[*i].path == path))
}

#[test]
fn hints_and_optional_offers_read_as_information_not_errors() {
    let rig = Rig::new();
    let mut o = overlay();
    o.set_hint("voice.tts.backend", "Programs: say built in".into());
    o.set_setup_offers(BTreeMap::from([(
        "voice.tts.backend".to_owned(),
        SetupOffer {
            id: "piper".into(),
            title: "Piper".into(),
            preview: "Piper: uv tool install piper-tts".into(),
        },
    )]));
    goto(&mut o, &rig.keys, "voice.tts.backend");
    let screen = draw(&mut o, &rig, 120, 30);
    assert!(screen.contains("Programs: say built in"), "{screen}");
    assert!(screen.contains("Ctrl+L installs Piper"), "{screen}");
    assert!(!screen.contains("not installed"), "{screen}");
}

#[test]
fn only_the_selected_memory_backend_shows_its_options() {
    let on_learning = |mut o: SettingsOverlay| {
        o.tab = o.tabs.iter().position(|(t, _)| *t == "Memory").unwrap();
        o
    };
    // Default provenance: no backend sections at all.
    let o = on_learning(overlay());
    assert!(shows(&o, "memory.backend") && shows(&o, "memory.autolearn"));
    for path in ["memory.hindsight.endpoint", "memory.local.extraction_role"] {
        assert!(!shows(&o, path), "{path} is noise for provenance");
    }
    let mut local = on_learning(SettingsOverlay::new(
        table("[memory]\nbackend = \"local\"\n"),
        Table::new(),
        table("[memory]\nbackend = \"local\"\n"),
        Scope::Global,
        vec![],
    ));
    assert!(shows(&local, "memory.local.extraction_role"));
    assert!(!shows(&local, "memory.hindsight.endpoint"));
    local.search = "Service URL".into();
    assert!(
        shows(&local, "memory.hindsight.endpoint"),
        "search still finds everything"
    );
    let off = on_learning(SettingsOverlay::new(
        table("[memory]\nbackend = \"off\"\n"),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec![],
    ));
    assert!(shows(&off, "memory.backend") && !shows(&off, "memory.autolearn"));
}

#[test]
fn choosing_a_backend_with_missing_fields_guides_through_them_before_switching() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "memory.backend");
    assert!(draw(&mut o, &rig, 110, 30).contains("Needs no setup"));
    // Pick "hindsight": nothing is saved (validation would reject it); we land on its URL.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let Mode::Pick {
        choices, cursor, ..
    } = &mut o.mode
    else {
        panic!()
    };
    *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert_eq!(path_at_cursor(&o), "memory.hindsight.endpoint");
    assert!(o.feedback.as_deref().unwrap().contains("Service URL"));
    assert!(!o.pending.contains_key("memory.backend"));
    // Fill it; once saved, the cursor returns to Backend with the choice ready.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send_all(&mut o, &rig.keys, typed("http://127.0.0.1:8888"));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    o.settled(&path, Ok(value));
    assert_eq!(path_at_cursor(&o), "memory.backend");
    assert!(
        o.feedback
            .as_deref()
            .unwrap()
            .contains("Hindsight service is ready")
    );
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), value.as_str()),
        ("memory.backend", Some("hindsight"))
    );
}

#[test]
fn backend_picker_starts_on_the_active_backend_after_a_guided_switch_is_abandoned() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "memory.backend");
    // Pick hindsight; its URL is missing, so the switch waits for it.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let Mode::Pick {
        choices, cursor, ..
    } = &mut o.mode
    else {
        panic!()
    };
    *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send_all(&mut o, &rig.keys, typed("http://127.0.0.1:8888"));
    let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), scope),
        ("memory.hindsight.endpoint", Scope::Global)
    );
    o.settled(&path, Ok(value));
    assert_eq!(path_at_cursor(&o), "memory.backend");
    // Open the picker (it preselects the pending hindsight), then cancel it.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    // Reopening starts on the backend that is actually active, not the abandoned choice.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    {
        let Mode::Pick {
            choices, cursor, ..
        } = &o.mode
        else {
            panic!("picker should reopen")
        };
        assert_eq!(choices[*cursor], "provenance");
    }
    // Choosing hindsight again and confirming saves exactly that backend, globally.
    {
        let Mode::Pick {
            choices, cursor, ..
        } = &mut o.mode
        else {
            panic!("picker should still be open")
        };
        *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
    }
    let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), &value, scope),
        (
            "memory.backend",
            &Value::String("hindsight".into()),
            Scope::Global
        )
    );
}

fn expect_set(event: OverlayEvent) -> (String, Value, Scope) {
    match event {
        OverlayEvent::Set { path, value, scope } => (path, value, scope),
        other => panic!("expected Set, got {other:?}"),
    }
}

fn choose_next(o: &mut SettingsOverlay, keys: &Keymap) -> OverlayEvent {
    assert!(matches!(
        send(o, keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(
        send(o, keys, key(KeyCode::Down)),
        OverlayEvent::None
    ));
    send(o, keys, key(KeyCode::Enter))
}

#[test]
fn renders_title_tabs_sections_rows_and_footer() {
    let rig = Rig::new();
    let mut o = overlay();
    let screen = draw(&mut o, &rig, 130, 30);
    for needle in [
        "\u{25c6} Settings",
        "Save to: All projects",
        "Appearance",
        "Interaction",
        "Setup",
        "Status Line",
        "Separator",
        "Powerline thin",
        "Preview: PREVIEW",
        "Type to search",
        "Esc close",
        "Ctrl+S scope",
    ] {
        assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
    }
    assert!(screen.starts_with('╭'), "consistent rounded frame");
}

#[test]
fn row_shows_modified_marker_and_source_per_layer() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\nstatus_separator = \"pipe\"\ntips = false\n"),
        table("[ui]\nstatus_separator = \"pipe\"\n"),
        table("[ui]\ntips = false\n"),
        Scope::Project,
        vec![],
    );
    let screen = draw(&mut o, &rig, 120, 40);
    let line = |needle: &str| {
        screen
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle}\n{screen}"))
            .to_owned()
    };
    let sep = line("Separator");
    assert!(sep.contains("Pipe") && sep.contains("project"), "{sep}");
    let tips = line(" Tips");
    assert!(tips.contains("off") && tips.contains("global"), "{tips}");
    let welcome = line("Welcome banner");
    assert!(
        welcome.contains("on") && !welcome.contains("project") && !welcome.contains("global"),
        "{welcome}"
    );
    assert!(screen.contains("Save to: This project"));
}

#[test]
fn auto_bool_picker_preselects_current_value_and_cancel_never_saves() {
    let rig = Rig::new();
    let path = "assistance.lean_local";
    for current in [
        Value::String("auto".into()),
        Value::Boolean(true),
        Value::Boolean(false),
    ] {
        let mut o = overlay();
        crate::config::schema::set(&mut o.effective, path, current.clone());
        goto(&mut o, &rig.keys, path);
        assert!(draw(&mut o, &rig, 120, 30).contains("Enter choose"));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        let Mode::Pick {
            choices, cursor, ..
        } = &o.mode
        else {
            panic!("automatic switches must open a picker")
        };
        assert_eq!(choice_value(Kind::AutoBool, &choices[*cursor]), current);
        let shown = draw(&mut o, &rig, 120, 30);
        for label in [
            "Auto (let Dial decide)",
            "On (enabled)",
            "Off (disabled)",
            "Esc cancel",
        ] {
            assert!(shown.contains(label), "missing {label}: {shown}");
        }
        // Confirming the current choice is also a no-op, including boolean values.
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Down)),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert!(matches!(o.mode, Mode::Browse));
        assert!(o.pending.is_empty());
        assert!(o.global.is_empty() && o.project.is_empty());
        assert_eq!(o.current_path(path), current);
    }
}

#[test]
fn auto_bool_picker_saves_typed_values_in_selected_scope_only_after_confirmation() {
    let rig = Rig::new();
    let path = "assistance.lean_local";
    for scope in [Scope::Global, Scope::Project] {
        let mut o = overlay();
        o.scope = scope;
        goto(&mut o, &rig.keys, path);
        for expected in [
            Value::Boolean(true),
            Value::Boolean(false),
            Value::String("auto".into()),
        ] {
            let before = o.current_path(path);
            assert!(matches!(
                send(&mut o, &rig.keys, key(KeyCode::Enter)),
                OverlayEvent::None
            ));
            assert!(matches!(
                send(&mut o, &rig.keys, key(KeyCode::Down)),
                OverlayEvent::None
            ));
            assert!(o.pending.is_empty(), "browsing must not save");
            assert_eq!(o.current_path(path), before);
            let (written, value, destination) =
                expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
            assert_eq!(
                (written.as_str(), &value, destination),
                (path, &expected, scope)
            );
            assert_eq!(
                crate::config::schema::get(&o.effective, path)
                    .cloned()
                    .unwrap_or(Value::String("auto".into())),
                before
            );
            o.settled(path, Ok(value));
            assert_eq!(
                crate::config::schema::get(o.layer_ref(scope), path),
                Some(&expected)
            );
            assert!(o.layer_ref(other_scope(scope)).is_empty());
            assert_eq!(o.current_path(path), expected);
        }
        let previous = o.current_path(path);
        let _ = expect_set(choose_next(&mut o, &rig.keys));
        o.settled(path, Err("Could not save".into()));
        assert_eq!(o.current_path(path), previous);
        assert_eq!(
            crate::config::schema::get(o.layer_ref(scope), path),
            Some(&previous)
        );
    }
}

#[test]
fn service_tier_rows_toggle_the_flags_the_slash_commands_write() {
    let rig = Rig::new();
    let mut o = overlay();
    // /fast, /slow and /extended-context persist these same keys; their rows write them too.
    for path in ["fast_mode", "slow_mode", "extended_context"] {
        goto(&mut o, &rig.keys, path);
        let (written, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!((written.as_str(), value), (path, Value::Boolean(true)));
    }
}

#[test]
fn bool_toggles_and_enum_choices_require_confirmation() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.tips");
    let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), value, scope),
        ("ui.tips", Value::Boolean(false), Scope::Global)
    );
    // Before the controller answers the displayed value stays on the pending value, then follows it.
    o.settled("ui.tips", Ok(Value::Boolean(false)));
    assert_eq!(
        crate::config::schema::get(&o.global, "ui.tips"),
        Some(&Value::Boolean(false)),
        "source layer updated"
    );
    let (_, value, _) = expect_set(send(&mut o, &rig.keys, ch(' ')));
    assert_eq!(value, Value::Boolean(true));

    goto(&mut o, &rig.keys, "ui.composer_shape");
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 100, 24).contains("Line"));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Down)),
        OverlayEvent::None
    ));
    assert!(
        !o.pending.contains_key("ui.composer_shape"),
        "browsing must not save"
    );
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Browse));
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("line".into()));
    o.settled("ui.composer_shape", Ok(value));
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("box".into()), "wraps around");
}

#[test]
fn number_input_validates_locally_and_esc_cancels() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "harness.max_steps");
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Edit { .. }));
    // The editor starts with the current value; replace it with out-of-range input.
    send_all(
        &mut o,
        &rig.keys,
        vec![Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ))],
    );
    send_all(&mut o, &rig.keys, typed("99"));
    assert!(
        matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ),
        "out of range is not sent"
    );
    let shown = draw(&mut o, &rig, 110, 30);
    assert!(shown.contains("Must be between 0 and 64"), "{shown}");
    assert!(matches!(o.mode, Mode::Edit { .. }), "stays in the editor");
    // Not a number at all.
    send_all(
        &mut o,
        &rig.keys,
        vec![key(KeyCode::Backspace), key(KeyCode::Backspace), ch('x')],
    );
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 110, 30).contains("Enter a whole number"));
    // A valid value goes out as an integer.
    send_all(
        &mut o,
        &rig.keys,
        vec![key(KeyCode::Backspace), ch('6'), ch('4')],
    );
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), value),
        ("harness.max_steps", Value::Integer(64))
    );
    assert!(matches!(o.mode, Mode::Browse));
    o.settled("harness.max_steps", Ok(Value::Integer(64)));
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(
        &mut o,
        &rig.keys,
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
    );
    send(&mut o, &rig.keys, ch('0'));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), value),
        ("harness.max_steps", Value::Integer(0))
    );
    o.settled("harness.max_steps", Ok(Value::Integer(0)));
    assert!(draw(&mut o, &rig, 110, 30).contains("0 (unlimited)"));
    assert_eq!(edit_text(&Value::Integer(0)), "0");
    // Esc leaves the editor without sending anything and without closing the panel.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Browse));
}

#[test]
fn float_and_list_inputs_and_step_adjustment() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "harness.temperature");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Up));
    let Mode::Edit { editor, .. } = &o.mode else {
        panic!()
    };
    assert_eq!(
        editor.text, "0.3",
        "Up adds one step to 0.2 without float noise"
    );
    let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(value, Value::Float(0.3));

    goto(&mut o, &rig.keys, "ui.status_left");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let Mode::Edit { editor, .. } = &o.mode else {
        panic!()
    };
    assert_eq!(editor.text, "spinner, model, path, git");
    send_all(
        &mut o,
        &rig.keys,
        vec![Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ))],
    );
    send_all(&mut o, &rig.keys, typed("model , ,context"));
    let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        value,
        Value::Array(vec![
            Value::String("model".into()),
            Value::String("context".into())
        ])
    );
}

#[test]
fn context_defaults_opens_the_shared_local_cap_and_saves_globally() {
    let rig = Rig::new();
    let mut o = overlay();
    o.scope = Scope::Project;
    let mut o = o.coding_only().context_defaults();
    let screen = draw(&mut o, &rig, 130, 30);
    assert!(screen.contains("Local context default"), "{screen}");
    assert!(screen.contains("Cloud context default"), "{screen}");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, ctrl('u'));
    send_all(&mut o, &rig.keys, typed("131072"));
    let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "local_context_window");
    assert_eq!(value, Value::Integer(131072));
    assert_eq!(scope, Scope::Global);
}

#[test]
fn context_editor_displays_auto_and_accepts_cloud_local_caps() {
    let rig = Rig::new();
    let mut o = overlay();
    for path in [
        "context_window",
        "cloud_context_window",
        "local_context_window",
    ] {
        goto(&mut o, &rig.keys, path);
        let screen = draw(&mut o, &rig, 130, 30);
        assert!(screen.contains("Auto"), "{screen}");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, ctrl('u'));
        send_all(&mut o, &rig.keys, typed("64000"));
        let (saved_path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(saved_path, path);
        assert_eq!(value, Value::Integer(64000));
        assert_eq!(scope, Scope::Global);
        o.settled(path, Ok(value));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, ctrl('u'));
        send_all(&mut o, &rig.keys, typed("AUTO"));
        let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(value.as_str(), Some("auto"));
        o.settled(path, Ok(value));
    }
    let kind = crate::config::schema::registry()
        .iter()
        .find(|d| d.path == "context_window")
        .unwrap()
        .kind;
    assert!(parse_input(kind, "2097153").is_err());
    assert!(parse_input(kind, "2047").is_err());
    assert_eq!(adjust(kind, "2048", -1).as_deref(), Some("auto"));
    assert_eq!(adjust(kind, "auto", 1).as_deref(), Some("2048"));
}

#[test]
fn delete_unsets_in_the_active_scope_only() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\ntips = false\n"),
        Table::new(),
        table("[ui]\ntips = false\n"),
        Scope::Project,
        vec![],
    );
    goto(&mut o, &rig.keys, "ui.tips");
    // Set in global, active scope is project: nothing to remove there, say why.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Delete)),
        OverlayEvent::None
    ));
    assert!(draw(&mut o, &rig, 120, 30).contains("Set in the global scope"));
    send(&mut o, &rig.keys, ctrl('s'));
    match send(&mut o, &rig.keys, key(KeyCode::Delete)) {
        OverlayEvent::Unset { path, scope } => {
            assert_eq!((path.as_str(), scope), ("ui.tips", Scope::Global))
        }
        other => panic!("{other:?}"),
    }
    o.settled("ui.tips", Ok(Value::Boolean(true)));
    assert!(crate::config::schema::get(&o.global, "ui.tips").is_none());
    let screen = draw(&mut o, &rig, 120, 30);
    let row = screen.lines().find(|l| l.contains(" Tips")).unwrap();
    assert!(
        row.contains("on") && !row.contains("global") && !row.contains("project"),
        "{row}"
    );
    // An unmodified row has nothing to reset.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Delete)),
        OverlayEvent::None
    ));
}

#[test]
fn search_filters_across_tabs_shows_location_and_escape_clears_before_closing() {
    let rig = Rig::new();
    let mut o = overlay();
    send_all(&mut o, &rig.keys, typed("sandbox"));
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("Sandbox mode") && shown.contains("Allow network"),
        "{shown}"
    );
    assert!(
        shown.contains("Tools/Sandbox"),
        "each hit names its tab and section\n{shown}"
    );
    assert!(
        !shown.contains("Welcome banner"),
        "non-matching rows are hidden"
    );
    assert!(shown.contains("Esc clear search"));
    // Hits come from other tabs than the current (Appearance) one.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(o.search.is_empty());
    // Appearance's Display rows sit below the fold at 30 rows with the framed chrome.
    assert!(draw(&mut o, &rig, 120, 40).contains("Welcome banner"));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::Close
    ));

    send_all(&mut o, &rig.keys, typed("zzzzqq"));
    assert!(draw(&mut o, &rig, 120, 30).contains("No settings match \"zzzzqq\""));
    send_all(&mut o, &rig.keys, vec![key(KeyCode::Backspace); 6]);
    assert!(o.search.is_empty());
    // Enter on a search hit acts on that hit, wherever it lives.
    send_all(&mut o, &rig.keys, typed("approval"));
    assert!(
        matches!(o.visible().first(), Some(Row::Setting(i)) if crate::config::schema::registry()[*i].path == "approval")
    );
    let (path, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(
        (path.as_str(), value),
        ("approval", Value::String("auto".into()))
    );
}

#[test]
fn search_matches_description_words_in_any_order_and_preserves_metadata_search() {
    let rig = Rig::new();
    let mut o = overlay();
    // A natural-language description of the setup link, not its title or location.
    send_all(&mut o, &rig.keys, typed("reachable remove"));
    assert!(o.visible().contains(&Row::Link(0)));
    o.search = "REMOVE reachable".into();
    assert!(o.visible().contains(&Row::Link(0)));

    let def = crate::config::schema::registry()
        .iter()
        .find(|d| d.path == "assistance.lean_local")
        .unwrap();
    // The complete description must be searchable without requiring metadata word order.
    o.search = def
        .description
        .split_whitespace()
        .rev()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(shows(&o, def.path));
    for query in [
        def.path.to_owned(),
        format!("{} {}", def.section, def.label),
        format!("{} {}", def.tab, def.label),
    ] {
        o.search = query;
        assert!(shows(&o, def.path), "{}", o.search);
    }
    assert!(search_match("sessions retain", "Memory", "Retain facts across sessions").is_some());
    assert!(
        search_match(
            "sessions impossible",
            "Memory",
            "Retain facts across sessions"
        )
        .is_none()
    );
    assert_eq!(
        search_match("network allow", "Allow network", ""),
        search_match("allow network", "Allow network", "")
    );
    assert_eq!(
        search_match("retain", "Memory", "Retain facts").unwrap().1,
        Vec::<usize>::new()
    );
}

#[test]
fn search_never_changes_scope_and_ctrl_s_preserves_the_query() {
    let rig = Rig::new();
    let mut o = overlay();
    send_all(&mut o, &rig.keys, typed("status"));
    assert_eq!(o.search, "status", "every letter belongs to search");
    assert_eq!(o.scope, Scope::Global);
    send(&mut o, &rig.keys, ctrl('s'));
    assert_eq!(o.scope, Scope::Project);
    assert_eq!(o.search, "status");
    assert!(draw(&mut o, &rig, 100, 24).contains("Save to: This project"));
    goto(&mut o, &rig.keys, "ui.tips");
    let (_, _, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(scope, Scope::Project, "later events carry the new scope");
    send(&mut o, &rig.keys, ctrl('s'));
    assert_eq!(o.scope, Scope::Global);
}

#[test]
fn settled_ok_updates_value_and_err_shows_inline_and_keeps_old_value() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.status");
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("minimal".into()));
    o.settled("ui.status", Err("status preset rejected".into()));
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(shown.contains("status preset rejected"), "{shown}");
    let row = shown.lines().find(|l| l.contains("Preset")).unwrap();
    assert!(row.contains("Default"), "old value kept: {row}");
    let error_style = style_of(
        &render(120, 30, |f| {
            let p = |u: &UiConfig, w| status_preview(u, w);
            let c = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &p,
            };
            o.draw(f, &c)
        }),
        "status preset rejected",
    )
    .unwrap();
    assert_eq!(
        error_style.fg,
        rig.theme.fg(Tok::Error).fg,
        "errors use the Error token"
    );
    // The next attempt clears the message; Ok then moves the value.
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("minimal".into()));
    assert!(!draw(&mut o, &rig, 120, 30).contains("status preset rejected"));
    o.settled("ui.status", Ok(Value::String("minimal".into())));
    assert!(draw(&mut o, &rig, 120, 30).contains("Saved to global settings"));
    let row = draw(&mut o, &rig, 120, 30)
        .lines()
        .find(|l| l.contains("Preset"))
        .unwrap()
        .to_owned();
    assert!(row.contains("Minimal") && row.contains("global"), "{row}");
}

#[test]
fn saved_scope_feedback_survives_controller_refresh_but_not_navigation() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\ntips = true"),
        table("[ui]\ntips = true"),
        Table::new(),
        Scope::Global,
        vec![],
    );
    goto(&mut o, &rig.keys, "ui.tips");
    expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert!(!draw(&mut o, &rig, 100, 24).contains("Saved globally"));
    o.settled("ui.tips", Ok(Value::Boolean(true)));
    o.replace_values(o.effective.clone(), o.project.clone(), o.global.clone());
    assert!(
        draw(&mut o, &rig, 100, 24)
            .contains("Saved globally; this project's override still applies")
    );
    send(&mut o, &rig.keys, key(KeyCode::Down));
    assert!(!draw(&mut o, &rig, 100, 24).contains("Saved globally"));
}

#[test]
fn short_panels_prioritize_search_and_non_appearance_settings_have_no_preview() {
    let rig = Rig::new();
    let mut o = overlay();
    let shown = draw(&mut o, &rig, 60, 8);
    assert!(shown.contains("Type to search all settings"), "{shown}");
    assert!(
        shown.contains("Enter choose") && shown.contains("Esc close"),
        "{shown}"
    );
    goto(&mut o, &rig.keys, "sandbox.mode");
    assert!(!draw(&mut o, &rig, 100, 24).contains("Preview:"));
}

#[test]
fn compact_choice_picker_keeps_the_highlighted_option_visible() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.theme");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    for height in [6, 8, 10, 12] {
        assert!(
            draw(&mut o, &rig, 50, height).contains("● auto"),
            "highlight hidden at height {height}"
        );
    }
    send(&mut o, &rig.keys, key(KeyCode::PageDown));
    for height in [6, 8, 10, 12] {
        assert!(
            draw(&mut o, &rig, 50, height).contains("● auto"),
            "highlight hidden at height {height}"
        );
    }
}

#[test]
fn preview_row_receives_the_pending_config() {
    let rig = Rig::new();
    let mut o = overlay();
    let seen: RefCell<Vec<UiConfig>> = RefCell::new(Vec::new());
    let preview = |ui: &UiConfig, _w: u16| {
        seen.borrow_mut().push(ui.clone());
        Line::from(format!("sep={:?}", ui.status_separator))
    };
    let paint = |o: &mut SettingsOverlay| {
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &preview,
        };
        screen(&render(120, 30, |f| o.draw(f, &ctx)))
    };
    assert!(paint(&mut o).contains("sep=PowerlineThin"));
    goto(&mut o, &rig.keys, "ui.status_separator");
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(
        value,
        Value::String("slash".into()),
        "next after powerline-thin"
    );
    let shown = paint(&mut o);
    assert!(
        shown.contains("Preview: sep=Slash"),
        "pending value is previewed before the controller answers\n{shown}"
    );
    o.settled("ui.status_separator", Err("nope".into()));
    assert!(
        paint(&mut o).contains("sep=PowerlineThin"),
        "a rejected change leaves the preview"
    );
    assert!(seen.borrow().len() >= 3);
}

#[test]
fn invalid_ui_table_falls_back_to_default_preview() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\nbogus = 1\n"),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec![],
    );
    let seen: RefCell<Option<UiConfig>> = RefCell::new(None);
    let preview = |ui: &UiConfig, _w: u16| {
        *seen.borrow_mut() = Some(ui.clone());
        Line::from("x")
    };
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let _ = render(100, 24, |f| o.draw(f, &ctx));
    assert_eq!(
        seen.borrow().as_ref().unwrap().status_separator,
        UiConfig::default().status_separator
    );
}

#[test]
fn theme_rows_pick_with_live_preview_and_cancel_restores() {
    let rig = Rig::new();
    // The picker starts on the active theme; pin it to `dark` (the default is now `auto`).
    let mut o = SettingsOverlay::new(
        table("[ui]\ntheme = \"dark\"\n"),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec!["dark".into(), "light".into(), "mono".into()],
    );
    goto(&mut o, &rig.keys, "ui.theme");
    assert!(
        matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ),
        "opening the picker emits nothing"
    );
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("○ light") && shown.contains("○ mono") && shown.contains("○ auto"),
        "{shown}"
    );
    match send(&mut o, &rig.keys, key(KeyCode::Down)) {
        OverlayEvent::PreviewTheme(Some(name)) => assert_eq!(name, "light"),
        other => panic!("{other:?}"),
    }
    match send(&mut o, &rig.keys, key(KeyCode::Down)) {
        OverlayEvent::PreviewTheme(Some(name)) => assert_eq!(name, "mono"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::PreviewTheme(None)
    ));
    assert!(matches!(o.mode, Mode::Browse), "Esc closes only the picker");

    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Down));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        (path.as_str(), value),
        ("ui.theme", Value::String("light".into()))
    );
    // Confirming the theme that is already active just ends the preview.
    o.settled("ui.theme", Ok(Value::String("light".into())));
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::PreviewTheme(None)
    ));
    // `auto` is only offered for the main theme, not for the dark/light slots.
    goto(&mut o, &rig.keys, "ui.theme_dark");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(
        shown.contains("○ mono") && !shown.contains("○ auto") && !shown.contains("● auto"),
        "{shown}"
    );
}

#[test]
fn tabs_and_sections_follow_the_keymap_including_remapped_keys() {
    let rig = Rig::new();
    let mut o = overlay();
    let tab_name =
        |o: &SettingsOverlay| o.tabs.get(o.tab).map(|(t, _)| *t).unwrap_or(PROVIDERS_TAB);
    send(&mut o, &rig.keys, key(KeyCode::Right));
    assert_eq!(tab_name(&o), "Interaction");
    send(&mut o, &rig.keys, key(KeyCode::Left));
    send(&mut o, &rig.keys, key(KeyCode::Left));
    assert_eq!(tab_name(&o), PROVIDERS_TAB, "wraps to the last tab");
    send(&mut o, &rig.keys, key(KeyCode::Right));
    assert_eq!(tab_name(&o), "Appearance");

    // Tab / Shift+Tab jump between sections of the tab.
    let section = |o: &SettingsOverlay| SettingsOverlay::section_of(o.current_row().unwrap());
    assert_eq!(section(&o), "Theme");
    send(&mut o, &rig.keys, key(KeyCode::Tab));
    assert_eq!(section(&o), "Symbols");
    send(&mut o, &rig.keys, key(KeyCode::Tab));
    assert_eq!(section(&o), "Composer");
    send(
        &mut o,
        &rig.keys,
        Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
    );
    assert_eq!(section(&o), "Symbols");
    for _ in 0..5 {
        send(&mut o, &rig.keys, key(KeyCode::Tab));
    }
    assert_eq!(section(&o), "Symbols", "wraps around the sections");

    // Remapped keymap: Ctrl+L / Ctrl+H switch tabs, Ctrl+J / Ctrl+K move, Ctrl+O confirms. Old keys do nothing.
    let mut remapped = Keymap::defaults();
    for (action, chord) in [
        (Action::TabNext, "Ctrl+L"),
        (Action::TabPrev, "Ctrl+H"),
        (Action::SelectDown, "Ctrl+J"),
        (Action::SelectUp, "Ctrl+K"),
        (Action::SelectConfirm, "Ctrl+O"),
        (Action::SectionNext, "Ctrl+N"),
    ] {
        remapped.rebind(action, &[chord]).unwrap();
    }
    let mut o = overlay();
    send(&mut o, &remapped, key(KeyCode::Right));
    assert_eq!(tab_name(&o), "Appearance", "default chord no longer bound");
    send(&mut o, &remapped, ctrl('l'));
    assert_eq!(tab_name(&o), "Interaction");
    send(&mut o, &remapped, ctrl('h'));
    assert_eq!(tab_name(&o), "Appearance");
    send(&mut o, &remapped, ctrl('j'));
    assert_eq!(o.cursor, 1);
    send(&mut o, &remapped, ctrl('k'));
    assert_eq!(o.cursor, 0);
    send(&mut o, &remapped, ctrl('n'));
    assert_eq!(section(&o), "Symbols");
    let hints = draw(
        &mut o,
        &Rig {
            keys: remapped.clone(),
            ..Rig::new()
        },
        140,
        30,
    );
    assert!(
        hints.contains("Ctrl+K/Ctrl+J move") && hints.contains("Ctrl+N sections"),
        "footer reflects the remap\n{hints}"
    );
    assert!(matches!(
        send(&mut o, &remapped, ctrl('o')),
        OverlayEvent::None
    ));
    assert!(matches!(
        send(&mut o, &remapped, ctrl('j')),
        OverlayEvent::None
    ));
    match send(&mut o, &remapped, ctrl('o')) {
        OverlayEvent::Set { path, .. } => assert_eq!(path, "ui.symbols"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn providers_tab_opens_the_other_screens() {
    let rig = Rig::new();
    let mut o = overlay();
    send(&mut o, &rig.keys, key(KeyCode::Left));
    let shown = draw(&mut o, &rig, 110, 24);
    assert!(
        shown.contains("Providers & API keys")
            && shown.contains("Models & roles")
            && shown.contains("Keyboard shortcuts"),
        "{shown}"
    );
    match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
        OverlayEvent::Action(a) => assert_eq!(a, "open:providers"),
        other => panic!("{other:?}"),
    }
    send(&mut o, &rig.keys, key(KeyCode::Down));
    match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
        OverlayEvent::Action(a) => assert_eq!(a, "open:models"),
        other => panic!("{other:?}"),
    }
    send(&mut o, &rig.keys, key(KeyCode::Down));
    match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
        OverlayEvent::Action(a) => assert_eq!(a, "open:shortcuts"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn restart_required_rows_are_marked() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "sandbox.mode");
    let shown = draw(&mut o, &rig, 130, 30);
    assert!(
        shown
            .lines()
            .find(|l| l.contains("Sandbox mode"))
            .unwrap()
            .contains("restart"),
        "{shown}"
    );
}

#[test]
fn long_lists_scroll_to_keep_the_cursor_visible_and_show_a_scrollbar() {
    let rig = Rig::new();
    // The local backend shows its own long list (`memory.local.*`); provenance shows none.
    let mut o = SettingsOverlay::new(
        table("[memory]\nbackend = \"local\"\n"),
        Table::new(),
        table("[memory]\nbackend = \"local\"\n"),
        Scope::Global,
        vec![],
    );
    o.tab = o.tabs.iter().position(|(t, _)| *t == "Memory").unwrap();
    o.advanced_open.insert(("Memory", "Rollout memory"));
    let shown = draw(&mut o, &rig, 120, 16);
    assert!(shown.contains(rig.theme.sym(Sym::ScrollThumb)), "{shown}");
    for _ in 0..20 {
        send(&mut o, &rig.keys, key(KeyCode::Down));
    }
    let shown = draw(&mut o, &rig, 120, 16);
    let label = match o.current_row() {
        Some(Row::Setting(i)) => crate::config::schema::registry()[i].label.to_owned(),
        Some(Row::Advanced(..)) => "advanced (".to_owned(),
        _ => panic!("cursor is on a row"),
    };
    assert!(
        shown.contains(&label),
        "cursor row {label:?} visible\n{shown}"
    );
    send(&mut o, &rig.keys, key(KeyCode::PageDown));
    assert!(o.cursor > 20);
    send(&mut o, &rig.keys, key(KeyCode::Up));
}

#[test]
fn replace_values_drops_pending_state() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.tips");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    o.settled("ui.tips", Err("x".into()));
    o.replace_values(table("[ui]\ntips = false\n"), Table::new(), Table::new());
    assert!(o.pending.is_empty() && o.notes.is_empty());
    assert!(
        draw(&mut o, &rig, 120, 30)
            .lines()
            .find(|l| l.contains(" Tips"))
            .unwrap()
            .contains("off")
    );
}

#[test]
fn tiny_terminals_never_panic_in_any_mode() {
    // Each width is an independent slice of the grid; threads keep the 961-cell sweep short
    // while every (width, height) pair still runs the same sequence.
    std::thread::scope(|scope| {
        for w in 0..=30u16 {
            scope.spawn(move || {
                let rig = Rig::new();
                for h in 0..=30u16 {
                    let mut o = overlay();
                    draw(&mut o, &rig, w, h);
                    goto(&mut o, &rig.keys, "harness.temperature");
                    send(&mut o, &rig.keys, key(KeyCode::Enter));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig.keys, key(KeyCode::Esc));
                    goto(&mut o, &rig.keys, "ui.theme");
                    send(&mut o, &rig.keys, key(KeyCode::Enter));
                    draw(&mut o, &rig, w, h);
                    send(&mut o, &rig.keys, key(KeyCode::Esc));
                    send_all(&mut o, &rig.keys, typed("ma"));
                    draw(&mut o, &rig, w, h);
                    send_all(&mut o, &rig.keys, typed("zzzz"));
                    draw(&mut o, &rig, w, h);
                }
            });
        }
    });
}

#[test]
fn wide_and_hostile_text_is_kept_inside_the_frame() {
    let rig = Rig::new();
    let mut o = SettingsOverlay::new(
        table("[ui]\nstatus_left = [\"日本語日本語日本語日本語\", \"\\u001b[31mred\"]\n"),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec![],
    );
    goto(&mut o, &rig.keys, "ui.status_left");
    let shown = draw(&mut o, &rig, 60, 20);
    assert!(!shown.contains('\u{1b}'));
    for line in shown.lines() {
        assert!(text::width(line) <= 80);
    }
}
#[test]
fn experimental_tab_renders_each_new_toggle_off_by_default() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "experimental.work_packet_contracts");
    let shown = draw(&mut o, &rig, 120, 32);
    for label in [
        "Work packet contracts",
        "Verification receipts",
        "Overlap deduplication",
        "Profile matching",
    ] {
        assert!(shown.contains(label), "missing {label} in:\n{shown}");
    }
    assert!(shown.contains("Experimental"));
    assert!(
        shown.contains("off"),
        "toggles should render disabled:\n{shown}"
    );
}

#[test]
fn settings_read_failure_is_visible_and_keeps_last_good_values() {
    let rig = Rig::new();
    let mut overlay = SettingsOverlay::new(
        table("[ui]\ntips=true"),
        Table::new(),
        Table::new(),
        Scope::Global,
        vec![],
    );
    overlay.banner("reading project settings: invalid TOML".into(), true);
    let shown = draw(&mut overlay, &rig, 120, 30);
    assert!(
        shown.contains("reading project settings: invalid TOML"),
        "{shown}"
    );
    assert_eq!(overlay.effective["ui"]["tips"].as_bool(), Some(true));
}
