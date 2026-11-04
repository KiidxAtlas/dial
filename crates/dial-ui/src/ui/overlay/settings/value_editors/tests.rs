use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press<E: Keyed>(editor: &mut E, code: KeyCode) -> Step {
    editor.press(&key(code))
}

trait Keyed {
    fn press(&mut self, key: &KeyEvent) -> Step;
}
impl Keyed for ListEditor {
    fn press(&mut self, key: &KeyEvent) -> Step {
        self.key(key, &Keymap::defaults())
    }
}
impl Keyed for EntriesEditor {
    fn press(&mut self, key: &KeyEvent) -> Step {
        self.key(key, &Keymap::defaults())
    }
}

fn type_text<E: Keyed>(editor: &mut E, text: &str) {
    for c in text.chars() {
        editor.press(&key(KeyCode::Char(c)));
    }
}

fn strings(values: &[&str]) -> Value {
    Value::Array(values.iter().map(|v| Value::String((*v).into())).collect())
}

fn known(values: &[&str]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|v| ((*v).to_owned(), (*v).to_owned()))
        .collect()
}

fn rows(view: View) -> Vec<String> {
    view.rows.into_iter().map(|(label, _)| label).collect()
}

#[test]
fn list_checks_offered_values_and_keeps_custom_ones() {
    let mut list = ListEditor::new(
        &strings(&["world", "custom"]),
        known(&["world", "experience", "observation"]),
        false,
        AddBy::Text,
        Path::new("/"),
    );
    assert_eq!(
        rows(list.view(20)),
        [
            "[x] world",
            "[x] custom",
            "[ ] experience",
            "[ ] observation",
            "+ Add another value…"
        ]
    );
    // Check "observation", uncheck "world".
    for _ in 0..3 {
        press(&mut list, KeyCode::Down);
    }
    press(&mut list, KeyCode::Char(' '));
    press(&mut list, KeyCode::Home);
    list.cursor = 0;
    press(&mut list, KeyCode::Char(' '));
    assert_eq!(
        press(&mut list, KeyCode::Enter),
        Step::Save(strings(&["custom", "observation"]))
    );
}

#[test]
fn ordered_lists_move_entries_with_shift_arrows() {
    let mut list = ListEditor::new(
        &strings(&["remote", "soft", "shake"]),
        known(&["remote", "soft", "shake", "snapcompact", "handoff"]),
        true,
        AddBy::Text,
        Path::new("/"),
    );
    assert_eq!(rows(list.view(3))[0], "[x] 1. remote");
    list.key(
        &KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
        &Keymap::defaults(),
    );
    assert_eq!(list.value(), strings(&["soft", "remote", "shake"]));
    list.key(
        &KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
        &Keymap::defaults(),
    );
    assert_eq!(list.value(), strings(&["remote", "soft", "shake"]));
}

#[test]
fn list_add_row_takes_typed_values_and_esc_discards_the_edit() {
    let mut list = ListEditor::new(
        &strings(&[]),
        known(&["codex"]),
        false,
        AddBy::Text,
        Path::new("/"),
    );
    press(&mut list, KeyCode::Up); // wraps to the add row
    press(&mut list, KeyCode::Enter);
    assert!(list.is_adding());
    type_text(&mut list, "mine");
    press(&mut list, KeyCode::Enter);
    assert!(!list.is_adding());
    assert_eq!(list.value(), strings(&["mine"]));
    assert_eq!(press(&mut list, KeyCode::Esc), Step::Cancel);
    let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(list.key(&ctrl_e, &Keymap::defaults()), Step::Type);
}

#[test]
fn folder_lists_add_through_the_browser() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("out")).unwrap();
    let mut list = ListEditor::new(&strings(&[]), Vec::new(), false, AddBy::Folder, root.path());
    assert_eq!(rows(list.view(5)), ["+ Add a folder…"]);
    press(&mut list, KeyCode::Enter);
    assert!(list.view(10).browser.is_some());
    type_text(&mut list, "out");
    press(&mut list, KeyCode::Enter); // opens out/
    press(&mut list, KeyCode::Enter); // "Use this folder"
    assert_eq!(
        list.value(),
        strings(&[root.path().join("out").to_str().unwrap()])
    );
}

#[test]
fn entries_edit_add_and_remove_one_table_entry_at_a_time() {
    let mut entries = EntriesEditor::new(toml::from_str::<Value>("a = 1\nb = \"x\"").unwrap());
    assert_eq!(
        rows(entries.view(10)),
        [
            "a = 1",
            "b = \"x\"",
            "+ Add an entry…",
            "Save (no changes yet)"
        ]
    );
    // Edit the first entry in place, renaming it.
    press(&mut entries, KeyCode::Enter);
    assert!(entries.is_editing());
    entries.input.as_mut().unwrap().1.set("c = { on = true }");
    press(&mut entries, KeyCode::Enter);
    // Add a new entry; a duplicate name is refused with a note.
    press(&mut entries, KeyCode::Down);
    press(&mut entries, KeyCode::Down);
    press(&mut entries, KeyCode::Enter);
    type_text(&mut entries, "b = 2");
    press(&mut entries, KeyCode::Enter);
    assert!(entries.view(10).note.unwrap().contains("already exists"));
    entries.input.as_mut().unwrap().1.set("d = [1, 2]");
    press(&mut entries, KeyCode::Enter);
    // Remove "b" (now the first row after sorting: b, c, d).
    entries.cursor = 0;
    press(&mut entries, KeyCode::Delete);
    entries.cursor = entries.len() - 1;
    let Step::Save(Value::Table(saved)) = press(&mut entries, KeyCode::Enter) else {
        panic!("expected a saved table");
    };
    assert_eq!(saved.keys().collect::<Vec<_>>(), ["c", "d"]);
    assert_eq!(saved["c"]["on"].as_bool(), Some(true));
}

#[test]
fn entries_edit_array_items_and_refuse_invalid_toml() {
    let mut entries = EntriesEditor::new(
        toml::from_str::<Table>("v = [{ name = \"check\" }]").unwrap()["v"].clone(),
    );
    assert_eq!(rows(entries.view(10))[0], "{ name = \"check\" }");
    press(&mut entries, KeyCode::Down);
    press(&mut entries, KeyCode::Enter);
    type_text(&mut entries, "{ name = ");
    press(&mut entries, KeyCode::Enter);
    assert_eq!(
        entries.view(10).note.as_deref(),
        Some("Write one TOML value")
    );
    press(&mut entries, KeyCode::Esc);
    assert!(!entries.is_editing());
    assert_eq!(press(&mut entries, KeyCode::Esc), Step::Cancel);
}

#[test]
fn nested_fields_preserve_unknown_siblings_and_advanced_toml() {
    for source in [
        "provider = { endpoint = \"old\", unknown = { flag = true }, token = \"local\" }",
        "v = [{ endpoint = \"old\", unknown = { flag = true }, token = \"local\" }]",
    ] {
        let table = toml::from_str::<Table>(source).unwrap();
        let seed = table.get("v").cloned().unwrap_or(Value::Table(table));
        let mut entries = EntriesEditor::new(seed.clone());
        press(&mut entries, KeyCode::Enter);
        let fields = &mut entries.fields.as_mut().unwrap().1;
        press(fields.as_mut(), KeyCode::Enter);
        fields.input.as_mut().unwrap().1.set("endpoint = \"new\"");
        press(fields.as_mut(), KeyCode::Enter);
        fields.cursor = fields.len() - 1;
        press(&mut entries, KeyCode::Enter);
        assert_eq!(entries.entry(0).unwrap()["endpoint"].as_str(), Some("new"));
        let original = match &seed {
            Value::Array(items) => &items[0],
            _ => &seed["provider"],
        };
        assert_eq!(entries.entry(0).unwrap()["unknown"], original["unknown"]);
        assert_eq!(entries.entry(0).unwrap()["token"].as_str(), Some("local"));
        press(&mut entries, KeyCode::Enter);
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(entries.key(&ctrl_e, &Keymap::defaults()), Step::Stay);
        assert!(entries.input.as_ref().unwrap().1.text.contains("unknown"));
        press(&mut entries, KeyCode::Esc);
        assert_eq!(entries.key(&ctrl_e, &Keymap::defaults()), Step::Type);
    }
}

#[test]
fn renaming_to_existing_entry_is_rejected_without_losing_either_value() {
    let seed = toml::from_str::<Value>("a = 1\nb = 2").unwrap();
    let mut entries = EntriesEditor::new(seed.clone());
    assert!(entries.commit(&Target::Entry(0), "b = 3").is_err());
    assert_eq!(entries.value(), &seed);
}

#[test]
fn focused_scalars_are_typed_without_toml_and_preserve_siblings() {
    for (source, text, expected) in [
        (
            "value = 'old'",
            "  plain \"text\"  ",
            Value::String("  plain \"text\"  ".into()),
        ),
        ("value = true", "false", Value::Boolean(false)),
        ("value = 22", "2022", Value::Integer(2022)),
        ("value = 0.2", "0.8", Value::Float(0.8)),
    ] {
        let seed = toml::from_str::<Value>(&format!(
            "record = {{ sibling = {{ unknown = true }}, {} }}",
            source
        ))
        .unwrap();
        let mut entries = EntriesEditor::for_setting(seed.clone(), true);
        press(&mut entries, KeyCode::Enter);
        let child = &mut entries.fields.as_mut().unwrap().1;
        child.cursor = 1; // sibling, value
        press(child.as_mut(), KeyCode::Enter);
        assert_eq!(
            child.input.as_ref().unwrap().1.text,
            seed["record"]["value"]
                .as_str()
                .map_or_else(|| inline(&seed["record"]["value"]), str::to_owned)
        );
        child.input.as_mut().unwrap().1.set(text);
        press(child.as_mut(), KeyCode::Enter);
        child.cursor = child.len() - 1;
        press(&mut entries, KeyCode::Enter);
        assert_eq!(entries.value()["record"]["value"], expected);
        assert_eq!(
            entries.value()["record"]["sibling"],
            seed["record"]["sibling"]
        );
        assert_eq!(press(&mut entries, KeyCode::Esc), Step::Cancel);
    }
}

#[test]
fn focused_nested_credentials_and_advanced_inputs_require_explicit_reveal() {
    for sensitive in [false, true] {
        let seed = toml::from_str::<Value>(
            "record = { headers = [{ auth = { api_key = 'sentinel-secret' } }] }",
        )
        .unwrap();
        let mut entries = EntriesEditor::for_setting(seed, sensitive);
        let assert_hidden = |editor: &EntriesEditor| {
            let view = editor.view(30);
            assert!(
                !view
                    .rows
                    .iter()
                    .any(|(text, _)| text.contains("sentinel-secret"))
            );
            assert!(
                !view
                    .input
                    .is_some_and(|(_, editor)| editor.text.contains("sentinel-secret"))
            );
        };
        assert_hidden(&entries);
        // record → headers array → first item → auth → api_key scalar.
        for _ in 0..5 {
            press(&mut entries, KeyCode::Enter);
            assert_hidden(&entries);
        }
        let reveal = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        entries.key(&reveal, &Keymap::defaults());
        assert_eq!(entries.view(30).input.unwrap().1.text, "sentinel-secret");
        entries.key(&reveal, &Keymap::defaults());
        assert_hidden(&entries);
        // Cancel scalar and auth, then use advanced TOML for the containing entry.
        press(&mut entries, KeyCode::Esc);
        press(&mut entries, KeyCode::Esc);
        entries.key(
            &KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &Keymap::defaults(),
        );
        assert_hidden(&entries);
        assert!(entries.view(30).input.is_some());
    }
}

#[test]
fn focused_invalid_scalar_keeps_original_and_reports_expected_type() {
    for (source, text, prompt) in [
        ("value = true", "yes", "true / false"),
        ("value = 22", "not a port", "Whole number"),
        ("value = 0.2", "NaN", "Number"),
    ] {
        let seed = toml::from_str::<Value>(source).unwrap();
        let mut entries = EntriesEditor::for_setting(seed.clone(), false);
        press(&mut entries, KeyCode::Enter);
        assert_eq!(entries.view(10).input.unwrap().0, prompt);
        entries.input.as_mut().unwrap().1.set(text);
        press(&mut entries, KeyCode::Enter);
        assert!(entries.view(10).note.is_some());
        assert_eq!(entries.value(), &seed);
    }
}

fn begin_creation(editor: &mut EntriesEditor, name: Option<&str>, kind: usize) {
    editor.cursor = editor.len() - 2;
    press(editor, KeyCode::Enter);
    if let Some(name) = name {
        editor.paste(name);
        press(editor, KeyCode::Enter);
    }
    for _ in 0..kind {
        press(editor, KeyCode::Down);
    }
    press(editor, KeyCode::Enter);
}

#[test]
fn typed_add_creates_each_scalar_in_empty_tables_and_arrays_without_toml() {
    for (kind, text, expected) in [
        (
            0,
            "  plain \"text\"  ",
            Value::String("  plain \"text\"  ".into()),
        ),
        (1, "true", Value::Boolean(true)),
        (2, "2022", Value::Integer(2022)),
        (3, "0.75", Value::Float(0.75)),
    ] {
        for table in [true, false] {
            let mut editor = EntriesEditor::for_setting(
                if table {
                    Value::Table(Table::new())
                } else {
                    Value::Array(Vec::new())
                },
                true,
            );
            begin_creation(&mut editor, table.then_some("new.field"), kind);
            editor.paste(text);
            assert!(!editor.view(20).input.unwrap().1.text.contains(text));
            editor.key(
                &KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                &Keymap::defaults(),
            );
            assert_eq!(editor.view(20).input.unwrap().1.text, text);
            press(&mut editor, KeyCode::Enter);
            let value = if table {
                &editor.value()["new.field"]
            } else {
                &editor.value().as_array().unwrap()[0]
            };
            assert_eq!(value, &expected);
            assert!(!rows(editor.view(20)).iter().any(|row| row.contains(text)));
            editor.cursor = editor.len() - 1;
            assert_eq!(
                press(&mut editor, KeyCode::Enter),
                Step::Save(editor.value().clone())
            );
        }
    }
}

#[test]
fn typed_add_builds_nested_records_and_arrays_without_inserting_cancelled_drafts() {
    for kind in [4, 5] {
        let seed = toml::from_str::<Value>("sibling = { untouched = true }").unwrap();
        let mut editor = EntriesEditor::for_setting(seed.clone(), false);
        begin_creation(&mut editor, Some("headers"), kind);
        let Some(Creating::Nested { editor: nested, .. }) = &mut editor.creating else {
            panic!("nested editor");
        };
        begin_creation(nested, (kind == 4).then_some("Authorization"), 0);
        nested.paste("sentinel-secret");
        press(nested.as_mut(), KeyCode::Enter);
        assert!(
            !rows(nested.view(20))
                .iter()
                .any(|row| row.contains("sentinel-secret"))
        );
        assert_eq!(editor.value(), &seed, "new record remains staged");
        let Some(Creating::Nested { editor: nested, .. }) = &mut editor.creating else {
            unreachable!();
        };
        nested.cursor = nested.len() - 1;
        press(&mut editor, KeyCode::Enter);
        let created = &editor.value()["headers"];
        assert_eq!(
            if kind == 4 {
                &created["Authorization"]
            } else {
                &created.as_array().unwrap()[0]
            },
            &Value::String("sentinel-secret".into())
        );
        assert_eq!(editor.value()["sibling"], seed["sibling"]);
        let accepted = editor.value().clone();
        begin_creation(&mut editor, Some("cancelled"), kind);
        press(&mut editor, KeyCode::Esc);
        assert_eq!(editor.value(), &accepted);
        assert_eq!(press(&mut editor, KeyCode::Esc), Step::Cancel);
    }
}

#[test]
fn typed_add_rejects_duplicate_names_and_invalid_values_and_keeps_advanced_escape() {
    let seed = toml::from_str::<Value>("existing = 42").unwrap();
    let mut editor = EntriesEditor::for_setting(seed.clone(), false);
    editor.cursor = editor.len() - 2;
    press(&mut editor, KeyCode::Enter);
    press(&mut editor, KeyCode::Enter);
    assert!(editor.view(20).note.unwrap().contains("name"));
    editor.paste("existing");
    press(&mut editor, KeyCode::Enter);
    assert!(editor.view(20).note.unwrap().contains("already exists"));
    press(&mut editor, KeyCode::Esc);
    for (kind, invalid) in [(1, "yes"), (2, "1.5"), (3, "NaN")] {
        begin_creation(&mut editor, Some("new"), kind);
        editor.paste(invalid);
        press(&mut editor, KeyCode::Enter);
        assert!(editor.view(20).note.is_some());
        assert_eq!(editor.value(), &seed);
        press(&mut editor, KeyCode::Esc);
    }
    begin_creation(&mut editor, Some("new"), 0);
    editor.paste("sentinel-secret");
    editor.key(
        &KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        &Keymap::defaults(),
    );
    assert!(
        editor
            .input
            .as_ref()
            .unwrap()
            .1
            .text
            .contains("sentinel-secret")
    );
    assert!(
        !editor
            .view(20)
            .input
            .unwrap()
            .1
            .text
            .contains("sentinel-secret")
    );
    press(&mut editor, KeyCode::Enter);
    assert_eq!(editor.value()["new"].as_str(), Some("sentinel-secret"));
}
