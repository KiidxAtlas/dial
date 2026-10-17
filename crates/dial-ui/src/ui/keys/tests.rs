use super::*;

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}
fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

fn native_alt(key: &str) -> String {
    format!(
        "{}+{key}",
        if cfg!(target_os = "macos") {
            "Option"
        } else {
            "Alt"
        }
    )
}

#[test]
fn native_labels_preserve_aliases_modifiers_and_config_spelling() {
    for name in ["Alt+M", "Option+M", "Meta+M"] {
        let chord = Chord::parse(name).unwrap();
        assert_eq!(chord.display(), native_alt("M"));
        assert_eq!(chord.config_name(), "Alt+M");
        assert!(chord.matches(&key(KeyCode::Char('m'), KeyModifiers::ALT)));
        assert!(!chord.matches(&key(KeyCode::Char('m'), KeyModifiers::CONTROL)));
        assert_eq!(Chord::parse(&chord.display()).unwrap(), chord);
    }
    let combined = Chord::parse("Control+Option+Shift+Enter").unwrap();
    assert_eq!(
        combined.display(),
        format!("Ctrl+{}", native_alt("Shift+Enter"))
    );
    assert_eq!(Chord::parse("Control+K").unwrap().display(), "Ctrl+K");
    assert!(Chord::parse("Command+K").is_err());

    let (map, warnings) =
        Keymap::from_table(&table(r#""app.model.select" = ["Alt+M", "Option+M"]"#));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(map.chords(Action::ModelSelect).len(), 1);
    assert_eq!(map.label(Action::ModelSelect), native_alt("M"));
    assert_eq!(map.labels(Action::ModelSelect), native_alt("M"));
    assert_eq!(
        map.hotkeys()
            .iter()
            .find(|r| r.id == Action::ModelSelect.id())
            .unwrap()
            .keys,
        native_alt("M")
    );
}

#[test]
fn every_action_has_a_unique_id_and_parsable_default_chords() {
    let mut ids = HashSet::new();
    for action in Action::ALL {
        assert!(ids.insert(action.id()), "duplicate {}", action.id());
        assert_eq!(Action::from_id(action.id()), Some(*action));
        for chord in action.defaults() {
            Chord::parse(chord).unwrap_or_else(|e| panic!("{}: {e}", action.id()));
        }
    }
}

#[test]
fn model_shortcut_is_platform_specific_and_keeps_alt_compatibility() {
    let map = Keymap::defaults();
    assert_eq!(
        map.label(Action::ModelSelect),
        if cfg!(target_os = "macos") {
            "Ctrl+K"
        } else {
            "Alt+M"
        }
    );
    assert_eq!(
        map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
        ),
        cfg!(target_os = "macos")
    );
    assert!(map.matches(
        Action::ModelSelect,
        &key(KeyCode::Char('m'), KeyModifiers::ALT)
    ));
    assert!(!map.matches(
        Action::ModelSelect,
        &key(KeyCode::Enter, KeyModifiers::NONE)
    ));
    assert!(!map.matches(
        Action::ModelSelect,
        &key(KeyCode::Char('µ'), KeyModifiers::NONE)
    ));
}

#[test]
fn chords_match_modifiers_exactly_and_backtab_is_shift_tab() {
    let alt_m = Chord::parse("Alt+M").unwrap();
    assert!(alt_m.matches(&key(KeyCode::Char('m'), KeyModifiers::ALT)));
    assert!(!alt_m.matches(&key(KeyCode::Char('m'), KeyModifiers::NONE)));
    assert!(!alt_m.matches(&key(
        KeyCode::Char('m'),
        KeyModifiers::ALT | KeyModifiers::CONTROL
    )));
    let shift_tab = Chord::parse("shift+tab").unwrap();
    assert!(
        shift_tab.matches(&key(KeyCode::BackTab, KeyModifiers::SHIFT))
            && shift_tab.matches(&key(KeyCode::BackTab, KeyModifiers::NONE))
    );
    assert!(
        !Chord::parse("Tab")
            .unwrap()
            .matches(&key(KeyCode::BackTab, KeyModifiers::SHIFT))
    );
    assert_eq!(alt_m.display(), native_alt("M"));
    let mut release = key(KeyCode::Char('m'), KeyModifiers::ALT);
    release.kind = KeyEventKind::Release;
    assert!(
        !alt_m.matches(&release),
        "key releases never trigger actions"
    );
}

#[test]
fn chord_parsing_rejects_nonsense_and_round_trips_special_keys() {
    for bad in ["", "Ctrl+", "Ctrl+Banana", "Alt++", "F99", "Ctrl"] {
        assert!(Chord::parse(bad).is_err(), "{bad:?}");
    }
    for good in [
        "Ctrl+Plus",
        "Alt+Shift+Enter",
        "Ctrl+,",
        "F5",
        "Shift+Tab",
        "Alt+Space",
        "PageDown",
        "Ctrl+Alt+Delete",
    ] {
        let chord = Chord::parse(good).unwrap();
        assert_eq!(Chord::parse(&chord.display()).unwrap(), chord, "{good}");
    }
    assert!(
        Chord::parse("Ctrl+Plus")
            .unwrap()
            .matches(&key(KeyCode::Char('+'), KeyModifiers::CONTROL))
    );
}

#[test]
fn user_file_overrides_adds_disables_and_reports_problems_without_failing() {
    let (map, warnings) = Keymap::from_table(&table(
        r#"
            "app.model.select" = "Ctrl+K"
            "tui.input.newline" = ["Shift+Enter", "Ctrl+Enter"]
            "app.history.search" = []
            "app.nonexistent" = "Ctrl+X"
            "app.tools.expand" = "Ctrl+Banana"
            "app.exit" = 42
            [app.provider]
            setup = "Alt+Shift+P"
        "#,
    ));
    assert!(map.matches(
        Action::ModelSelect,
        &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
    ));
    assert!(
        !map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('m'), KeyModifiers::ALT)
        ),
        "the default is replaced, not added to"
    );
    assert_eq!(map.chords(Action::Newline).len(), 2);
    assert!(
        map.chords(Action::HistorySearch).is_empty(),
        "an empty list disables the action"
    );
    assert_eq!(map.label(Action::HistorySearch), "");
    assert_eq!(
        map.label(Action::ProviderSetup),
        native_alt("Shift+P"),
        "nested tables name the same action id"
    );
    assert_eq!(
        map.label(Action::ToolsExpand),
        "Ctrl+O",
        "an invalid chord keeps the default"
    );
    assert_eq!(map.label(Action::Exit), "Ctrl+D");
    let all = warnings.join("\n");
    assert!(
        all.contains("unknown action") && all.contains("Ctrl+Banana") && all.contains("app.exit"),
        "{all}"
    );
}

#[test]
fn conflicts_resolve_deterministically_in_the_users_favor() {
    // A default binding of another action gives way to the user's choice.
    let (map, warnings) = Keymap::from_table(&table(r#""app.model.roles" = "Alt+M""#));
    assert_eq!(map.label(Action::ModelRoles), native_alt("M"));
    assert!(
        !map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('m'), KeyModifiers::ALT)
        ),
        "Alt+M moved off its default owner without removing other chords"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("moved from app.model.select")),
        "{warnings:?}"
    );
    // Two user bindings claiming one chord: the earlier action keeps it, the other is told.
    let (map, warnings) = Keymap::from_table(&table(
        "\"app.model.select\" = \"Ctrl+K\"\n\"app.settings\" = \"Ctrl+K\"",
    ));
    assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
    assert!(map.chords(Action::Settings).is_empty());
    assert!(
        warnings.iter().any(|w| w.contains("already bound")),
        "{warnings:?}"
    );
    // Different contexts may share a chord: Right completes in the composer and switches tabs in panels.
    let (map, _) = Keymap::from_table(&table(r#""tui.tabs.next" = "Right""#));
    assert!(
        map.matches(Action::Complete, &key(KeyCode::Right, KeyModifiers::NONE))
            && map.matches(Action::TabNext, &key(KeyCode::Right, KeyModifiers::NONE))
    );
    // Esc legitimately serves dismiss, interrupt and clear at once.
    let defaults = Keymap::defaults();
    let esc = key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        defaults.actions_for(&esc).contains(&Action::Dismiss)
            && defaults.actions_for(&esc).contains(&Action::Interrupt)
    );
}

#[test]
fn rebind_reset_and_customization_tracking() {
    let mut map = Keymap::defaults();
    map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
    assert!(map.is_customized(Action::ModelSelect));
    assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
    assert!(map.matches(
        Action::ModelSelect,
        &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
    ));
    assert!(map.rebind(Action::ModelSelect, &["Ctrl+Banana"]).is_err());
    assert_eq!(
        map.label(Action::ModelSelect),
        "Ctrl+K",
        "a failed rebind changes nothing"
    );
    assert!(
        map.rebind(Action::Settings, &["Ctrl+K"])
            .unwrap_err()
            .contains("already bound to app.model.select")
    );
    map.rebind(Action::Settings, &["Alt+M"]).unwrap();
    assert_eq!(
        map.label(Action::Settings),
        native_alt("M"),
        "a free default chord can be taken"
    );
    map.reset(Action::ModelSelect);
    assert!(!map.is_customized(Action::ModelSelect));
    assert_eq!(
        map.label(Action::ModelSelect),
        Keymap::defaults().label(Action::ModelSelect)
    );
}

#[test]
fn saved_bindings_round_trip_and_a_corrupt_file_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mut map = Keymap::defaults();
    map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
    map.rebind(Action::Newline, &["Alt+Enter", "Ctrl+Plus"])
        .unwrap();
    map.rebind(Action::HistorySearch, &[]).unwrap();
    map.save(dir.path()).unwrap();
    let (loaded, warnings) = Keymap::load(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    for action in [
        Action::ModelSelect,
        Action::Newline,
        Action::HistorySearch,
        Action::Settings,
    ] {
        assert_eq!(loaded.chords(action), map.chords(action), "{action:?}");
    }
    assert!(loaded.is_customized(Action::HistorySearch) && !loaded.is_customized(Action::Settings));
    let text = fs::read_to_string(dir.path().join("keybindings.toml")).unwrap();
    let saved: toml::Table = text.parse().unwrap();
    assert_eq!(
        saved[Action::Newline.id()].as_array().unwrap()[0].as_str(),
        Some("Alt+Enter")
    );
    assert!(
        text.contains("app.model.select") && !text.contains("app.settings"),
        "only customized actions are written:\n{text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.path().join("keybindings.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::write(
        dir.path().join("keybindings.toml"),
        "app.model.select = [oops",
    )
    .unwrap();
    assert!(map.save(dir.path()).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("keybindings.toml")).unwrap(),
        "app.model.select = [oops",
        "the user's file is untouched"
    );
    let (fallback, warnings) = Keymap::load(dir.path());
    assert_eq!(
        fallback.label(Action::ModelSelect),
        Keymap::defaults().label(Action::ModelSelect)
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("using default key bindings"))
    );
}

#[test]
fn interactive_edits_persist_before_applying_and_reject_collisions_or_lockout() {
    let dir = tempfile::tempdir().unwrap();
    let mut map = Keymap::defaults();
    map.edit_and_save(dir.path(), Action::ModelSelect, &["Ctrl+K"])
        .unwrap();
    assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
    assert_eq!(
        Keymap::load(dir.path()).0.label(Action::ModelSelect),
        "Ctrl+K"
    );
    let path = dir.path().join("keybindings.toml");
    let before = fs::read_to_string(&path).unwrap();
    for (action, chords) in [
        (Action::HistorySearch, vec!["Ctrl+K"]),
        (Action::HistorySearch, vec!["Alt+P"]),
        (Action::SelectConfirm, vec![]),
        (Action::SelectCancel, vec![]),
        (Action::ModelSelect, vec!["Ctrl+Banana"]),
    ] {
        let old = map.chords(action).to_vec();
        assert!(map.edit_and_save(dir.path(), action, &chords).is_err());
        assert_eq!(map.chords(action), old);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
    map.edit_and_save(dir.path(), Action::HistorySearch, &[])
        .unwrap();
    assert!(map.chords(Action::HistorySearch).is_empty());
    map.edit_and_save(
        dir.path(),
        Action::HistorySearch,
        Action::HistorySearch.defaults(),
    )
    .unwrap();
    assert!(
        !map.is_customized(Action::HistorySearch),
        "restoring defaults clears the modified marker"
    );
    assert_eq!(
        Keymap::load(dir.path()).0.label(Action::HistorySearch),
        "Ctrl+R"
    );
    assert!(
        !toml::from_str::<toml::Table>(&fs::read_to_string(&path).unwrap())
            .unwrap()
            .contains_key(Action::HistorySearch.id())
    );
    fs::write(&path, "not valid = [").unwrap();
    assert!(
        map.edit_and_save(dir.path(), Action::ModelSelect, &["Ctrl+Y"])
            .is_err()
    );
    assert_eq!(
        map.label(Action::ModelSelect),
        "Ctrl+K",
        "failed writes leave live shortcuts unchanged"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "not valid = [");
}

#[test]
fn hotkeys_table_reflects_remaps_and_unbound_actions() {
    let mut map = Keymap::defaults();
    map.rebind(Action::HistorySearch, &[]).unwrap();
    map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
    let rows = map.hotkeys();
    assert_eq!(rows.len(), Action::ALL.len());
    let find = |id: &str| rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(find("app.history.search").keys, "unbound");
    assert!(find("app.model.select").customized && find("app.model.select").keys == "Ctrl+K");
    assert_eq!(find("app.settings").keys, "Ctrl+, / Ctrl+4");
    assert!(!find("app.settings").customized);
    assert_eq!(find("tui.select.up").group, "Navigation");
}

#[test]
fn editor_actions_form_their_own_context_without_default_collisions() {
    let grouped: Vec<Action> = Action::ALL
        .iter()
        .copied()
        .filter(|a| a.group() == "Editor")
        .collect();
    assert_eq!(grouped, Action::EDITOR);
    let map = Keymap::defaults();
    let mut seen = HashSet::new();
    for action in Action::EDITOR {
        assert_eq!(action.context(), 2);
        for chord in map.chords(*action) {
            assert!(
                seen.insert(*chord),
                "{} reused in the editor",
                chord.display()
            );
        }
    }
    // A user may give an editing action a chord an application shortcut also uses.
    let (map, warnings) = Keymap::from_table(&table(r#""tui.editor.undo" = "Ctrl+P""#));
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(map.matches(
        Action::EditorUndo,
        &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
    ));
    assert!(map.matches(
        Action::CommandPalette,
        &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
    ));
    assert!(map.matches(
        Action::EditorJumpForward,
        &key(KeyCode::Char('5'), KeyModifiers::CONTROL)
    ));
}

fn release(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    let mut event = KeyEvent::new(code, mods);
    event.kind = KeyEventKind::Release;
    event
}

#[test]
fn voice_release_matches_bound_key_even_when_alt_is_already_up() {
    let map = Keymap::defaults();
    // Alt is often released before the letter; the release must still match.
    assert!(map.release_matches(
        Action::VoiceInput,
        &release(KeyCode::Char('v'), KeyModifiers::NONE)
    ));
    assert!(map.release_matches(
        Action::VoiceInput,
        &release(KeyCode::Char('V'), KeyModifiers::ALT)
    ));
    // Presses are not releases.
    assert!(!map.release_matches(
        Action::VoiceInput,
        &key(KeyCode::Char('v'), KeyModifiers::ALT)
    ));
    // Other keys do not release the voice action.
    assert!(!map.release_matches(
        Action::VoiceInput,
        &release(KeyCode::Char('x'), KeyModifiers::NONE)
    ));
}
