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
