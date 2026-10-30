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

/// Enter on a setting, then Ctrl+E when Enter opened a chooser or structured editor, so the test
/// ends in the plain text editor either way.
fn open_text(o: &mut SettingsOverlay, keys: &Keymap) {
    send(o, keys, key(KeyCode::Enter));
    if !matches!(o.mode, Mode::Edit { .. }) {
        assert!(
            matches!(
                o.mode,
                Mode::List { .. } | Mode::Entries { .. } | Mode::Path { .. }
            ),
            "Enter opened neither a text nor a structured editor"
        );
        send(o, keys, ctrl('e'));
    }
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
fn voice_tab_shows_dictation_without_search_or_advanced_and_allows_enabling_it() {
    let rig = Rig::new();
    for coding_only in [false, true] {
        let mut o = if coding_only {
            overlay().coding_only()
        } else {
            overlay()
        };
        o.tab = o.tabs.iter().position(|(tab, _)| *tab == "Voice").unwrap();
        for path in [
            "voice.stt.enabled",
            "voice.stt.backend",
            "voice.stt.input_mode",
            "voice.stt.submit_trigger",
            "voice.tts.auto_speak",
        ] {
            assert!(
                shows(&o, path),
                "{path} missing (coding_only={coding_only})"
            );
        }
        let screen = draw(&mut o, &rig, 100, 30);
        assert!(screen.contains("Dictation"), "{screen}");
        assert!(screen.contains("Recognition program"), "{screen}");
        for (path, value) in [
            ("voice.stt.input_mode", Value::String("hold".into())),
            ("voice.stt.submit_trigger", Value::String("never".into())),
            ("voice.tts.auto_speak", Value::Boolean(true)),
        ] {
            goto(&mut o, &rig.keys, path);
            send(&mut o, &rig.keys, key(KeyCode::Enter));
            if value.is_str() {
                send(&mut o, &rig.keys, key(KeyCode::Home));
                send(&mut o, &rig.keys, key(KeyCode::Down));
                send(&mut o, &rig.keys, key(KeyCode::Enter));
            }
            assert!(
                matches!(o.pending.get(path), Some(Pending::Set(actual, _)) if actual == &value),
                "{path} did not save the selected value"
            );
        }
        o.cursor = o.visible().iter().position(|row| matches!(row, Row::Setting(i) if crate::config::schema::registry()[*i].path == "voice.stt.enabled")).unwrap();
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        assert!(matches!(
            o.pending.get("voice.stt.enabled"),
            Some(Pending::Set(Value::Boolean(true), _))
        ));
    }
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
        " Settings",
        "Save to: All projects",
        "Appearance",
        "Interaction",
        "Setup",
        "Status Line",
        "Separator",
        "Powerline thin",
        "Preview: PREVIEW",
        "Search: ",
        "Esc close",
        "Ctrl+S scope",
    ] {
        assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
    }
    assert!(
        screen.starts_with(" Settings") && !screen.contains('╭'),
        "a borderless panel titled on its first row"
    );
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
        // Start explicitly at Auto; the product default may be On or Off.
        crate::config::schema::set(&mut o.effective, path, Value::String("auto".into()));
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
    assert_eq!(value, Value::String("box".into()));
    o.settled("ui.composer_shape", Ok(value));
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("line".into()));
    o.settled("ui.composer_shape", Ok(value));
    let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
    assert_eq!(value, Value::String("band".into()), "wraps around");
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
    assert!(
        matches!(o.mode, Mode::List { .. }),
        "known-value lists open the checklist"
    );
    send(&mut o, &rig.keys, ctrl('e'));
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
    assert!(shown.contains("Search: "), "{shown}");
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

#[test]
fn empty_search_names_the_key_that_leaves_it() {
    let rig = Rig::new();
    let mut o = overlay();
    send_all(&mut o, &rig.keys, typed("zzzzqq"));
    let shown = draw(&mut o, &rig, 100, 24);
    assert!(
        shown.contains("No settings match \"zzzzqq\". Esc clears the search."),
        "{shown}"
    );
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    assert!(!draw(&mut o, &rig, 100, 24).contains("No settings match"));
}

fn theme_overlay(themes: &[&str]) -> SettingsOverlay {
    SettingsOverlay::new(
        Table::new(),
        Table::new(),
        Table::new(),
        Scope::Global,
        themes.iter().map(|t| (*t).to_owned()).collect(),
    )
}

fn open_theme_picker(o: &mut SettingsOverlay, keys: &Keymap) {
    goto(o, keys, "ui.theme");
    assert!(matches!(
        send(o, keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Pick { .. }), "picker should open");
}

fn filter_text(o: &SettingsOverlay) -> String {
    match &o.mode {
        Mode::Pick { filter, .. } => filter.text.clone(),
        _ => panic!("picker should be open"),
    }
}

fn picker_value(o: &SettingsOverlay) -> &str {
    match &o.mode {
        Mode::Pick {
            choices, cursor, ..
        } => choices[*cursor].as_str(),
        _ => panic!("picker should be open"),
    }
}

#[test]
fn picker_filter_narrows_previews_the_match_and_esc_clears_before_closing() {
    let rig = Rig::new();
    let mut o = theme_overlay(&["dark", "light", "mono"]);
    open_theme_picker(&mut o, &rig.keys);
    // "m" leaves only mono, so the selection moves onto it and the theme previews.
    match send(&mut o, &rig.keys, ch('m')) {
        OverlayEvent::PreviewTheme(Some(name)) => assert_eq!(name, "mono"),
        other => panic!("{other:?}"),
    }
    assert_eq!(picker_value(&o), "mono");
    let shown = draw(&mut o, &rig, 100, 30);
    assert!(shown.contains("1 of 4"), "{shown}");
    // Esc clears the filter first and keeps the picker open on the same value.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Pick { .. }));
    assert_eq!(filter_text(&o), "");
    assert_eq!(picker_value(&o), "mono");
    // Only a bare Esc closes it, restoring the saved theme preview.
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::PreviewTheme(None)
    ));
    assert!(matches!(o.mode, Mode::Browse));
}

#[test]
fn picker_filter_matches_unicode_case_insensitively_and_saves_the_raw_value() {
    let rig = Rig::new();
    let mut o = theme_overlay(&["dark", "Éclair", "mono"]);
    open_theme_picker(&mut o, &rig.keys);
    send_all(&mut o, &rig.keys, typed("Éa"));
    // Backspace removes one whole character, never half of a multi-byte one.
    send(&mut o, &rig.keys, key(KeyCode::Backspace));
    assert_eq!(filter_text(&o), "É");
    send(&mut o, &rig.keys, ch('c'));
    assert_eq!(picker_value(&o), "Éclair");
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "ui.theme");
    assert_eq!(value.as_str(), Some("Éclair"), "the raw value is saved");
}

#[test]
fn picker_filter_matches_labels_but_saves_the_raw_value() {
    let rig = Rig::new();
    let mut o = overlay();
    let path = "assistance.lean_local";
    crate::config::schema::set(&mut o.effective, path, Value::Boolean(true));
    goto(&mut o, &rig.keys, path);
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    // "decide" appears only in the label "Auto (let Dial decide)", never in the raw value.
    send_all(&mut o, &rig.keys, typed("decide"));
    let shown = draw(&mut o, &rig, 120, 30);
    assert!(shown.contains("Auto (let Dial decide)"), "{shown}");
    assert!(!shown.contains("Off (disabled)"), "{shown}");
    let (saved, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(saved, path);
    assert_eq!(value, choice_value(Kind::AutoBool, "auto"));
}

#[test]
fn picker_filter_with_no_matches_saves_nothing_and_says_how_to_recover() {
    let rig = Rig::new();
    let mut o = theme_overlay(&["dark", "light", "mono"]);
    open_theme_picker(&mut o, &rig.keys);
    send_all(&mut o, &rig.keys, typed("zzz"));
    let shown = draw(&mut o, &rig, 100, 30);
    assert!(shown.contains("No option matches \"zzz\""), "{shown}");
    assert!(shown.contains("clear filter"), "{shown}");
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Pick { .. }), "still picking");
}

#[test]
fn long_pickers_offer_a_filter_and_each_new_picker_starts_unfiltered() {
    let rig = Rig::new();
    let mut o = theme_overlay(&[
        "dark", "light", "mono", "ember", "forest", "ocean", "rose", "slate", "violet",
    ]);
    open_theme_picker(&mut o, &rig.keys);
    let shown = draw(&mut o, &rig, 100, 30);
    assert!(shown.contains("10 options"), "{shown}");
    assert!(shown.contains("Type filter"), "{shown}");
    send_all(&mut o, &rig.keys, typed("o"));
    assert!(draw(&mut o, &rig, 100, 30).contains(" of 10"));
    // Esc clears the filter, a second Esc closes the picker, and reopening starts unfiltered.
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    assert!(matches!(o.mode, Mode::Browse));
    open_theme_picker(&mut o, &rig.keys);
    assert_eq!(filter_text(&o), "");
    assert!(draw(&mut o, &rig, 100, 30).contains("10 options"));
}

fn draw_with(
    o: &mut SettingsOverlay,
    rig: &Rig,
    w: u16,
    h: u16,
    preview: &dyn Fn(&UiConfig, u16) -> Line<'static>,
) -> String {
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: preview,
    };
    screen(&render(w, h, |f| o.draw(f, &ctx)))
}

#[test]
fn only_the_selected_row_carries_the_selection_marker() {
    let rig = Rig::new();
    let mut o = overlay();
    let shown = draw(&mut o, &rig, 120, 30);
    let pointer = rig.theme.sym(Sym::Pointer);
    assert_eq!(shown.matches(pointer).count(), 1, "{shown}");
    let marked = shown.lines().find(|l| l.contains(pointer)).unwrap();
    assert!(
        !marked.contains("Search"),
        "the search field is not a selected row: {marked}"
    );
}

#[test]
fn appearance_rows_hide_the_preview_line_when_there_is_nothing_to_preview() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.theme");
    let empty = |_: &UiConfig, _: u16| Line::default();
    assert!(!draw_with(&mut o, &rig, 100, 24, &empty).contains("Preview:"));
    assert!(draw(&mut o, &rig, 100, 24).contains("Preview: PREVIEW"));
}

#[test]
fn narrow_title_sheds_the_scope_label_without_touching_the_title() {
    let rig = Rig::new();
    let mut o = overlay();
    for w in [24u16, 30, 38, 45, 60, 120] {
        let shown = draw(&mut o, &rig, w, 16);
        let top = shown.lines().next().unwrap();
        assert!(top.starts_with(" Settings "), "width {w}: {top}");
        if w >= 60 {
            assert!(top.contains("Ctrl+S"), "width {w}: {top}");
        }
    }
    // Switching scope is acknowledged in the body, where every width can see it.
    send(&mut o, &rig.keys, ctrl('s'));
    assert!(draw(&mut o, &rig, 38, 16).contains("this project"));
}

#[test]
fn clipped_tab_strip_marks_hidden_tabs_and_keeps_the_active_one_whole() {
    let theme = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let rig = Rig::with_theme(theme);
    let mut o = overlay();
    let names: Vec<&str> = o.tabs.iter().map(|(t, _)| *t).collect();
    let first = draw(&mut o, &rig, 38, 16);
    let strip = first.lines().nth(1).unwrap();
    assert!(strip.trim_end().ends_with("..."), "{strip}");
    // Every name on the strip is whole: no tab is clipped mid-word.
    for word in strip.split_whitespace().filter(|w| *w != "...") {
        assert!(
            names.contains(&word) || word == PROVIDERS_TAB,
            "clipped {word:?} in {strip}"
        );
    }
    for _ in 0..names.len() {
        send(&mut o, &rig.keys, key(KeyCode::Right));
    }
    let last = draw(&mut o, &rig, 38, 16);
    let strip = last.lines().nth(1).unwrap();
    assert!(strip.trim_start().starts_with("..."), "{strip}");
    assert!(strip.contains(PROVIDERS_TAB), "{strip}");
}

#[test]
fn filtered_choices_show_every_match_that_fits() {
    let rig = Rig::new();
    let themes: Vec<String> = (0..12).map(|n| format!("dark-{n}")).collect();
    let refs: Vec<&str> = themes.iter().map(String::as_str).collect();
    let mut o = theme_overlay(&refs);
    open_theme_picker(&mut o, &rig.keys);
    o.handle(&Event::Paste("dark".into()), &rig.keys);
    let shown = draw(&mut o, &rig, 100, 40);
    for theme in &themes {
        assert!(shown.contains(theme.as_str()), "{theme} missing:\n{shown}");
    }
}

#[test]
fn home_and_end_jump_in_the_list_and_in_pickers() {
    let rig = Rig::new();
    let mut o = overlay();
    let last = o.visible().len() - 1;
    send(&mut o, &rig.keys, key(KeyCode::End));
    assert_eq!(o.cursor, last);
    send(&mut o, &rig.keys, key(KeyCode::Home));
    assert_eq!(o.cursor, 0);
    let mut o = theme_overlay(&["dark", "light", "mono", "ember"]);
    open_theme_picker(&mut o, &rig.keys);
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::End)),
        OverlayEvent::PreviewTheme(Some(theme)) if theme == "auto"
    ));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Home)),
        OverlayEvent::PreviewTheme(Some(theme)) if theme == "dark"
    ));
}

#[test]
fn structured_settings_roundtrip_nested_tables_and_arrays_without_stringifying() {
    for (path, source) in [
        (
            "verification",
            "{ commands = [{ name = \"check\", command = \"cargo check\", timeout_secs = 30 }] }",
        ),
        (
            "model_profiles",
            "{ \"custom/profile\" = { provider = \"custom\", model = \"model\", max_tokens = 16384 } }",
        ),
        (
            "hooks",
            "[{ event = \"turn_end\", command = \"echo done\" }]",
        ),
        (
            "providers",
            "{ custom = { base_url = \"http://localhost:8000/v1\", headers = [{ name = \"X-Token\", value = { env = \"CUSTOM_TOKEN\" } }] } }",
        ),
    ] {
        let def = crate::config::schema::registry()
            .iter()
            .find(|def| def.path == path)
            .unwrap();
        let value = parse_input(def.kind, source).unwrap();
        let text = setting_edit_text(def, &value);
        assert_eq!(parse_input(def.kind, &text).unwrap(), value, "{path}");
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, path);
        open_text(&mut o, &rig.keys);
        let Mode::Edit { editor, .. } = &mut o.mode else {
            panic!("{path} did not open an editor");
        };
        editor.set(source);
        assert!(
            matches!(send(&mut o, &rig.keys, key(KeyCode::Enter)), OverlayEvent::Set { path: written, value: saved, scope: Scope::Global } if written == path && saved == value)
        );
    }
    assert!(parse_input(Kind::Toml, "{}\nunrelated = true").is_err());
    assert!(parse_input(Kind::Toml, "{ invalid = }").is_err());
}

#[test]
fn exact_configuration_path_is_ranked_before_fuzzy_matches() {
    let mut o = overlay();
    for (index, def) in crate::config::schema::registry().iter().enumerate() {
        o.search = def.path.to_uppercase();
        assert_eq!(
            o.visible().first(),
            Some(&Row::Setting(index)),
            "{}",
            def.path
        );
    }
}

#[test]
fn credential_settings_are_masked_until_deliberately_revealed() {
    let rig = Rig::new();
    for (path, source) in [
        ("secrets.values", "[\"sentinel-private-secret\"]"),
        (
            "mcp",
            "{ custom = { env = { TOKEN = \"sentinel-private-secret\" } } }",
        ),
        (
            "model_roles",
            "{ small = { provider = \"custom\", model = \"m\", headers = [{ name = \"X-Token\", value = \"sentinel-private-secret\" }] } }",
        ),
        (
            "model_favorites",
            "[{ provider = \"custom\", model = \"m\", headers = [{ name = \"X-Token\", value = \"sentinel-private-secret\" }] }]",
        ),
        (
            "model_presets",
            "{ custom = { roles = { small = { headers = [{ name = \"X-Token\", value = \"sentinel-private-secret\" }] } } } }",
        ),
    ] {
        let mut o = overlay();
        let def = crate::config::schema::registry()
            .iter()
            .find(|def| def.path == path)
            .unwrap();
        let value: Value = toml::from_str::<Table>(&format!("value = {source}"))
            .unwrap()
            .remove("value")
            .unwrap();
        crate::config::schema::set(&mut o.effective, path, value.clone());
        crate::config::schema::set(&mut o.global, path, value.clone());
        goto(&mut o, &rig.keys, path);
        assert!(!draw(&mut o, &rig, 180, 40).contains("sentinel-private-secret"));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        assert!(!draw(&mut o, &rig, 180, 40).contains("sentinel-private-secret"));
        send(&mut o, &rig.keys, ctrl('r'));
        assert!(o.reveal_sensitive);
        assert!(draw(&mut o, &rig, 220, 40).contains("sentinel-private-secret"));
        send(&mut o, &rig.keys, key(KeyCode::Esc));
        assert!(!draw(&mut o, &rig, 180, 40).contains("sentinel-private-secret"));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        assert!(!o.reveal_sensitive);
        assert_eq!(o.current(def), value);
    }
}

#[test]
fn every_registry_setting_is_reachable_in_coding_or_learning_view() {
    let mut coding = overlay().coding_only();
    let mut learning = overlay().learning_only();
    for (index, def) in crate::config::schema::registry().iter().enumerate() {
        coding.search = def.path.into();
        learning.search = def.path.into();
        assert!(
            [&coding, &learning]
                .iter()
                .any(|o| o.visible().contains(&Row::Setting(index))),
            "{} is unreachable",
            def.path
        );
    }
}

#[test]
fn descendant_path_search_finds_structured_management_routes() {
    let mut o = overlay().coding_only();
    for (query, route) in [
        ("browser.allowed_hosts", "browser"),
        ("providers.custom.headers", "providers"),
        ("verification.commands", "verification"),
    ] {
        o.search = query.into();
        assert!(o.visible().iter().any(|row| matches!(row, Row::Setting(i) if crate::config::schema::registry()[*i].path == route)), "{query}");
    }
}

#[test]
fn project_structured_edits_do_not_copy_global_credentials_or_unrelated_fields() {
    let rig = Rig::new();
    for path in ["providers", "mcp", "browser"] {
        let inherited =
            table("[inherited]\ncredential = 'private-global-only'\nunrelated = true\n");
        let local = table("[project_only]\nsetting = 'local-value'\n");
        for project_value in [None, Some(Value::Table(local.clone()))] {
            let mut o = overlay();
            o.scope = Scope::Project;
            crate::config::schema::set(&mut o.global, path, Value::Table(inherited.clone()));
            let mut merged = inherited.clone();
            if let Some(Value::Table(project)) = &project_value {
                merged.extend(project.clone());
                crate::config::schema::set(&mut o.project, path, Value::Table(project.clone()));
            }
            crate::config::schema::set(&mut o.effective, path, Value::Table(merged));
            goto(&mut o, &rig.keys, path);
            send(&mut o, &rig.keys, key(KeyCode::Enter));
            if let Mode::Entries { editor, .. } = &o.mode {
                let working = editor.value().to_string();
                assert!(!working.contains("private-global-only"), "{path}");
                assert!(!working.contains("unrelated"), "{path}");
                send(&mut o, &rig.keys, ctrl('e'));
            }
            let Mode::Edit { editor, def } = &o.mode else {
                panic!("expected structured editor");
            };
            assert!(!editor.text.contains("private-global-only"));
            assert!(!editor.text.contains("unrelated"));
            let expected = project_value.unwrap_or_else(|| Value::Table(Table::new()));
            assert_eq!(parse_input(def.kind, &editor.text).unwrap(), expected);
            assert!(
                matches!(send(&mut o, &rig.keys, key(KeyCode::Enter)), OverlayEvent::Set { value, scope: Scope::Project, .. } if value == expected)
            );
        }
    }
}

#[test]
fn structured_seeds_preserve_selected_scope_pending_values_and_empty_array_shape() {
    let mut o = overlay();
    for path in ["providers", "hooks", "model_favorites"] {
        let def = crate::config::schema::registry()
            .iter()
            .find(|def| def.path == path)
            .unwrap();
        let inherited = def.default_value().unwrap();
        crate::config::schema::set(&mut o.global, path, inherited);
        o.scope = Scope::Project;
        let empty = if path == "providers" {
            Value::Table(Table::new())
        } else {
            Value::Array(Vec::new())
        };
        assert_eq!(o.edit_seed(def), empty, "{path}");
        let pending = if path == "providers" {
            Value::Table(table("[pending]\nmodel = 'local'"))
        } else {
            Value::Array(vec![Value::String("local".into())])
        };
        o.pending
            .insert(path.into(), Pending::Set(pending.clone(), Scope::Project));
        assert_eq!(o.edit_seed(def), pending);
        o.pending
            .insert(path.into(), Pending::Set(pending, Scope::Global));
        assert_eq!(
            o.edit_seed(def),
            empty,
            "other-scope pending must not be copied"
        );
        o.pending
            .insert(path.into(), Pending::Unset(Scope::Project));
        assert_eq!(o.edit_seed(def), empty);
    }
}

#[test]
fn path_settings_open_a_browser_and_save_only_the_chosen_entry() {
    let rig = Rig::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("venv")).unwrap();
    let python = dir.path().join("python3");
    std::fs::write(&python, "").unwrap();
    let mut o = overlay();
    o.set_base_dir(dir.path().to_path_buf());
    goto(&mut o, &rig.keys, "python");
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert!(
        matches!(o.mode, Mode::Path { .. }),
        "Enter browses instead of typing"
    );
    let screen = draw(&mut o, &rig, 120, 40);
    assert!(screen.contains("python3"), "{screen}");
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Esc)),
        OverlayEvent::None
    ));
    assert!(matches!(o.mode, Mode::Browse), "Esc writes nothing");

    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send_all(&mut o, &rig.keys, typed("python3"));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "python");
    assert_eq!(value, Value::String(python.display().to_string()));

    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, ctrl('e'));
    assert!(
        matches!(o.mode, Mode::Edit { .. }),
        "Ctrl+E keeps typing available"
    );
}

#[test]
fn known_value_lists_check_values_instead_of_typing_them() {
    let rig = Rig::new();
    let mut o = overlay();
    goto(&mut o, &rig.keys, "ui.status_right");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let Mode::List { editor, .. } = &o.mode else {
        panic!("expected the checklist");
    };
    let before = editor.value();
    let screen = draw(&mut o, &rig, 120, 60);
    assert!(
        screen.contains("[ ] "),
        "unchosen segments are offered: {screen}"
    );
    // Space on the first row flips it; Enter saves the whole list.
    send(&mut o, &rig.keys, key(KeyCode::Char(' ')));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "ui.status_right");
    assert_ne!(value, before);
    for segment in value.as_array().unwrap() {
        let segment = segment.as_str().unwrap();
        assert!(
            crate::config::schema::STATUS_SEGMENTS.contains(&segment),
            "{segment} is not a known status segment"
        );
    }
}

#[test]
fn collections_open_a_per_entry_editor_that_saves_the_whole_value() {
    let rig = Rig::new();
    let mut o = overlay();
    // Collections that can hold credentials (hooks, MCP, providers…) keep the masked text editor.
    goto(&mut o, &rig.keys, "hooks");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(
        matches!(o.mode, Mode::Edit { .. }),
        "sensitive stays masked"
    );
    send(&mut o, &rig.keys, key(KeyCode::Esc));

    goto(&mut o, &rig.keys, "verification");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(
        matches!(o.mode, Mode::Entries { .. }),
        "tables list entries"
    );
    // An empty table shows the add row first; Enter opens its input.
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send_all(
        &mut o,
        &rig.keys,
        typed("commands = [{ name = \"check\", command = \"cargo check\" }]"),
    );
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let screen = draw(&mut o, &rig, 120, 40);
    assert!(screen.contains("cargo check"), "{screen}");
    assert!(screen.contains("Save changes"), "{screen}");
    // Down past the add row reaches Save.
    send(&mut o, &rig.keys, key(KeyCode::Down));
    send(&mut o, &rig.keys, key(KeyCode::Down));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "verification");
    assert_eq!(
        value,
        parse_input(
            Kind::Toml,
            "{ commands = [{ name = \"check\", command = \"cargo check\" }] }"
        )
        .unwrap()
    );
}

#[test]
fn model_offers_ask_before_downloading_and_esc_disarms() {
    let rig = Rig::new();
    let mut o = overlay();
    o.set_value_choices(BTreeMap::from([(
        "local_runtime.draft_model".to_owned(),
        vec![
            ValueChoice {
                value: "org/ready".into(),
                label: "Ready model".into(),
                action: ChoiceAction::Set,
            },
            ValueChoice {
                value: "org/remote".into(),
                label: "Remote model".into(),
                action: ChoiceAction::Download,
            },
        ],
    )]));
    goto(&mut o, &rig.keys, "local_runtime.draft_model");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(matches!(o.mode, Mode::Pick { .. }), "models are chosen");
    send(&mut o, &rig.keys, key(KeyCode::Down));
    assert!(
        matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ),
        "the first Enter only asks"
    );
    assert!(matches!(o.mode, Mode::Pick { .. }));
    assert!(o.feedback.as_deref().unwrap().contains("Enter again"));
    send(&mut o, &rig.keys, key(KeyCode::Esc));
    assert!(o.download_armed.is_none(), "Esc disarms");

    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Down));
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::DownloadSetting { path, value, scope: Scope::Global }
            if path == "local_runtime.draft_model" && value == "org/remote"
    ));
    assert!(matches!(o.mode, Mode::Browse));

    send(&mut o, &rig.keys, key(KeyCode::Enter));
    let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(
        value,
        Value::String("org/ready".into()),
        "ready models save at once"
    );
}

#[test]
fn scalar_override_still_starts_from_inherited_effective_value() {
    let mut o = overlay();
    o.scope = Scope::Project;
    let def = crate::config::schema::registry()
        .iter()
        .find(|def| def.path == "python")
        .unwrap();
    let value = Value::String("/global/python".into());
    crate::config::schema::set(&mut o.effective, def.path, value.clone());
    assert_eq!(o.edit_seed(def), value);
}

#[test]
fn voice_choices_confirm_install_without_changing_selection() {
    let rig = Rig::new();
    for path in ["voice.tts.backend", "voice.stt.backend"] {
        let mut o = overlay();
        o.scope = Scope::Project;
        let value = if path == "voice.tts.backend" {
            "piper"
        } else {
            "whisper-cpp"
        };
        o.set_value_choices(BTreeMap::from([(
            path.into(),
            vec![ValueChoice {
                value: value.into(),
                label: "Missing backend".into(),
                action: ChoiceAction::Install {
                    preview: "Install runtime and voice model".into(),
                },
            }],
        )]));
        goto(&mut o, &rig.keys, path);
        let previous = o.current_path(path);
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        // The current Auto choice follows the missing choice supplied by the controller.
        send(&mut o, &rig.keys, key(KeyCode::Home));
        send(&mut o, &rig.keys, key(KeyCode::Char('M')));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert_eq!(o.current_path(path), previous);
        assert!(matches!(o.mode, Mode::Pick { .. }));
        assert!(draw(&mut o, &rig, 120, 32).contains("Enter again"));
        send(&mut o, &rig.keys, key(KeyCode::Esc));
        assert!(o.download_armed.is_none());
        assert_eq!(o.current_path(path), previous);
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, key(KeyCode::Home));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::InstallVoice { path: picked, value: backend, scope: Scope::Project }
                if picked == path && backend == value));
        assert_eq!(
            o.current_path(path),
            previous,
            "controller saves only after success"
        );
    }
}

#[test]
fn voice_choices_reuse_ready_backends_and_explain_manual_setup() {
    let rig = Rig::new();
    let mut o = overlay();
    o.set_value_choices(BTreeMap::from([(
        "voice.tts.backend".into(),
        vec![
            ValueChoice {
                value: "espeak".into(),
                label: "Ready".into(),
                action: ChoiceAction::Set,
            },
            ValueChoice {
                value: "openai".into(),
                label: "OpenAI".into(),
                action: ChoiceAction::Unavailable {
                    reason: "Configure an API key in Providers".into(),
                },
            },
        ],
    )]));
    goto(&mut o, &rig.keys, "voice.tts.backend");
    send(&mut o, &rig.keys, key(KeyCode::Enter));
    send(&mut o, &rig.keys, key(KeyCode::Home));
    send(&mut o, &rig.keys, key(KeyCode::Down));
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    assert_eq!(
        o.feedback.as_deref(),
        Some("Configure an API key in Providers")
    );
    assert!(matches!(
        send(&mut o, &rig.keys, key(KeyCode::Enter)),
        OverlayEvent::None
    ));
    send(&mut o, &rig.keys, key(KeyCode::Home));
    let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
    assert_eq!(path, "voice.tts.backend");
    assert_eq!(value.as_str(), Some("espeak"));
}
