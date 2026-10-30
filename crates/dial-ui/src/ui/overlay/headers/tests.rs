use super::*;

fn headers(count: usize) -> Option<RequestHeaders> {
    Some(
        (0..count)
            .map(|index| RequestHeader {
                name: format!("X-Header-{index}"),
                value: if index % 2 == 0 {
                    HeaderValue::Env(format!("TOKEN_{index}"))
                } else {
                    HeaderValue::Literal(format!("value-{index}"))
                },
            })
            .collect(),
    )
}
fn text(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}
fn cells(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn no_line_is_wider_than_the_panel_at_any_width() {
    let theme = Theme::dark();
    let mut editor = HeaderEditor::new(&headers(5));
    editor.error =
        Some("a deliberately long validation message that must be clipped to fit".into());
    for width in [1, 8, 20, 40, 80, 160] {
        for height in [1, 4, 8, 20] {
            let lines = editor.lines(&theme, width, height);
            assert!(lines.len() <= height.max(1), "{width}x{height}");
            for line in &lines {
                assert!(cells(line) <= width, "{width}x{height}: {line:?}");
            }
        }
    }
}

#[test]
fn rows_show_kind_name_value_and_the_selection_pointer() {
    let theme = Theme::dark();
    let editor = HeaderEditor::new(&headers(2));
    let rows = text(&editor.lines(&theme, 80, 20));
    assert!(
        rows[0].contains("HTTP HEADERS") && rows[0].contains("2/32"),
        "{rows:?}"
    );
    let first = rows.iter().find(|row| row.contains("X-Header-0")).unwrap();
    assert!(
        first.contains("env")
            && first.contains("TOKEN_0")
            && first.contains(theme.sym(Sym::Pointer))
    );
    let second = rows.iter().find(|row| row.contains("X-Header-1")).unwrap();
    assert!(
        second.contains("literal")
            && second.contains("value-1")
            && !second.contains(theme.sym(Sym::Pointer))
    );
}

#[test]
fn the_selected_row_is_highlighted_and_stays_visible_when_the_list_scrolls() {
    let theme = Theme::dark();
    let mut editor = HeaderEditor::new(&headers(12));
    for _ in 0..10 {
        editor.key(&key(KeyCode::Down, KeyModifiers::NONE), &Keymap::defaults());
    }
    let lines = editor.lines(&theme, 80, 12);
    let rows = text(&lines);
    assert!(
        rows.iter().any(|row| row.contains("X-Header-10")),
        "{rows:?}"
    );
    assert!(rows.last().unwrap().contains("row 11 of 12"), "{rows:?}");
    let selected = lines
        .iter()
        .position(|line| text(std::slice::from_ref(line))[0].contains("X-Header-10"))
        .unwrap();
    assert_eq!(lines[selected].style.bg, theme.bg(Tok::SelectedBg).bg);
    assert_ne!(lines[selected - 1].style.bg, theme.bg(Tok::SelectedBg).bg);
}

#[test]
fn empty_states_explain_what_saving_will_do() {
    let theme = Theme::dark();
    let inherit = text(&HeaderEditor::new(&None).lines(&theme, 80, 20));
    assert!(
        inherit.iter().any(|row| row.contains("Inheriting"))
            && inherit.iter().any(|row| row.contains("No header rows"))
    );
    let mut cleared = HeaderEditor::new(&None);
    cleared.key(
        &key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        &Keymap::defaults(),
    );
    assert!(
        text(&cleared.lines(&theme, 80, 20))
            .iter()
            .any(|row| row.contains("clears every inherited header"))
    );
}

#[test]
fn literal_rows_warn_and_validation_errors_are_shown() {
    let theme = Theme::dark();
    let mut editor = HeaderEditor::new(&headers(2));
    assert!(
        text(&editor.lines(&theme, 100, 20))
            .iter()
            .any(|row| row.contains("unencrypted"))
    );
    editor.key(
        &key(KeyCode::Char('n'), KeyModifiers::CONTROL),
        &Keymap::defaults(),
    );
    assert!(matches!(
        editor.key(
            &key(KeyCode::F(10), KeyModifiers::NONE),
            &Keymap::defaults()
        ),
        Outcome::Pending
    ));
    let rows = text(&editor.lines(&theme, 100, 20));
    assert!(rows.iter().any(|row| row.starts_with(" ! ")), "{rows:?}");
    assert!(
        rows.iter().any(|row| row.contains("(value)")),
        "inactive empty field shows its placeholder: {rows:?}"
    );
}

#[test]
fn f9_inherits_and_tab_only_switches_fields() {
    let theme = Theme::dark();
    let keymap = Keymap::defaults();
    let mut editor = HeaderEditor::new(&headers(2));
    editor.key(&key(KeyCode::Tab, KeyModifiers::NONE), &keymap);
    assert!(editor.value_field, "Tab moves between name and value");
    assert_eq!(editor.rows.len(), 2, "Tab must not clear headers");
    assert!(matches!(
        editor.key(&key(KeyCode::F(9), KeyModifiers::NONE), &keymap),
        Outcome::Pending
    ));
    assert!(editor.inherit && editor.rows.is_empty());
    assert!(matches!(
        editor.key(&key(KeyCode::F(10), KeyModifiers::NONE), &keymap),
        Outcome::Save(None)
    ));
    assert!(
        text(&HeaderEditor::new(&headers(1)).lines(&theme, 100, 20))
            .iter()
            .any(|row| row.contains("F9 inherit"))
    );
}

#[test]
fn ctrl_s_saves_where_f10_is_taken_by_the_terminal() {
    let keymap = Keymap::defaults();
    let mut editor = HeaderEditor::new(&headers(1));
    assert!(matches!(
        editor.key(&key(KeyCode::Char('s'), KeyModifiers::CONTROL), &keymap),
        Outcome::Save(Some(rows)) if rows.len() == 1
    ));
    assert!(
        text(&editor.lines(&Theme::dark(), 100, 6))
            .iter()
            .any(|row| row.contains("^S save")),
        "the compact hint names the portable save key"
    );
}

#[test]
fn title_names_the_model_and_role_being_edited() {
    let theme = Theme::dark();
    let editor = HeaderEditor::new(&None).titled("qwen → role main".into());
    let rows = text(&editor.lines(&theme, 100, 20));
    assert!(rows[0].contains("qwen → role main"), "{rows:?}");
}
