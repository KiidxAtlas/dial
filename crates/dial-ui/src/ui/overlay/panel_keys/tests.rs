use super::*;
use crate::ui::theme::Theme;
use crossterm::event::KeyModifiers;

fn event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn plain(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn help_does_not_steal_punctuation_from_text_entry() {
    for code in [KeyCode::Char('?'), KeyCode::Char('/')] {
        assert!(!PanelKeys::default().handle_key(&event(code), true));
    }
    let mut panel = PanelKeys::default();
    assert!(panel.handle_key(&event(KeyCode::Char('?')), false));
    assert!(panel.open());
    assert!(panel.handle_key(&event(KeyCode::Char('x')), false));
    assert!(panel.handle_key(&event(KeyCode::Esc), false));
    assert!(!panel.open());
    assert!(!panel.handle_key(&event(KeyCode::Char('x')), false));
    // `/` always reaches the panel, which decides whether it opens a search or is typed.
    assert!(!panel.handle_key(&event(KeyCode::Char('/')), false));
    assert!(panel.handle_key(&event(KeyCode::Char('?')), false));
    assert!(panel.handle_key(&event(KeyCode::Enter), false));
    assert!(panel.handle_key(&event(KeyCode::F(1)), true));
}

#[test]
fn key_sheet_lists_both_arrows_tabs_and_search_only_when_offered() {
    let keys = Keymap::defaults();
    let rows = PanelKeys::rows(&keys, false, false);
    assert_eq!(rows[0].0, "Up/Down");
    assert!(rows.iter().all(|(k, _)| k != "/" && k != "Tab/Shift+Tab"));
    assert_eq!(rows.last().unwrap().0, "Esc", "the way out is last");
    let rows = PanelKeys::rows(&keys, true, true);
    assert!(rows.iter().any(|(k, _)| k == "/"));
    assert!(rows.iter().any(|(k, _)| k == "Tab/Shift+Tab"));
}

#[test]
fn sheet_aligns_keys_wraps_notes_and_ends_with_the_way_back() {
    let theme = Theme::dark();
    let groups = [SheetGroup {
        title: "Move",
        rows: vec![
            ("Up/Down".into(), "move".into()),
            ("Enter".into(), "open".into()),
        ],
    }];
    let lines = plain(&sheet(
        &theme,
        &groups,
        "first note that is long enough to wrap\nsecond",
        "Esc",
        20,
    ));
    assert_eq!(lines[0], " MOVE");
    assert_eq!(lines[1], "   Up/Down  move");
    assert_eq!(lines[2], "   Enter    open");
    assert!(lines.iter().any(|l| l.trim() == "second"));
    assert!(lines.iter().all(|l| crate::ui::text::width(l) <= 20));
    assert_eq!(lines.last().unwrap(), " Esc back to panel");
}

#[test]
fn a_long_footer_keeps_the_action_the_finder_the_sheet_and_the_way_out() {
    let s = |v: &[&str]| v.iter().map(|h| h.to_string()).collect::<Vec<_>>();
    let short = s(&["Enter save", "Esc cancel"]);
    assert_eq!(capped_footer(short.clone(), "F1 keys"), short);
    let long = s(&[
        "Enter set main",
        "",
        "F7 tune",
        "Shift+F2 filter",
        "type to search",
        "Ctrl+B scope",
        "Esc close",
    ]);
    assert_eq!(
        capped_footer(long, "F1 keys"),
        s(&["Enter set main", "type to search", "F1 keys", "Esc close"])
    );
    let picker = s(&[
        "Enter save",
        "↑↓ preview",
        "Type filter",
        "Tab x",
        "Esc cancel",
    ]);
    assert_eq!(
        capped_footer(picker, "F1 keys")[1],
        "Type filter",
        "filtering outranks previewing"
    );
    let plain = s(&["Enter open", "↑↓ move", "a add", "x remove", "Esc close"]);
    let capped = capped_footer(plain, "F1 keys");
    assert_eq!(capped[1], "↑↓ move");
    assert_eq!(capped.len(), FOOTER_HINTS);
}

#[test]
fn key_rows_fit_narrow_panels_without_losing_their_text() {
    let theme = Theme::dark();
    let lines = plain(&sheet(
        &theme,
        &[SheetGroup {
            title: "Actions",
            rows: vec![(
                "Ctrl+PageDown".into(),
                "move to the next section safely".into(),
            )],
        }],
        "",
        "Esc",
        12,
    ));
    assert!(
        lines.iter().all(|line| text::width(line) <= 12),
        "{lines:?}"
    );
    assert!(lines.join(" ").contains("safely"));
}

#[test]
fn help_scrolls_to_last_note_with_back_pinned_even_on_short_panels() {
    let theme = Theme::dark();
    let mut panel = PanelKeys::default();
    panel.handle_key(&event(KeyCode::F(1)), false);
    let content = || {
        sheet(
            &theme,
            &[SheetGroup {
                title: "Keys",
                rows: vec![("Enter".into(), "apply".into())],
            }],
            "first note\nlast note",
            "Esc",
            20,
        )
    };
    let first = plain(&panel.viewport(content(), 20, 3, &theme));
    assert!(first.last().unwrap().contains("Esc"));
    panel.handle_key(&event(KeyCode::End), false);
    let last = plain(&panel.viewport(content(), 20, 3, &theme));
    assert!(
        last.iter().any(|line| line.contains("last note")),
        "{last:?}"
    );
    assert!(last.last().unwrap().contains("Esc"));
    let tiny = plain(&panel.viewport(content(), 20, 1, &theme));
    assert_eq!(tiny.len(), 1);
    assert!(tiny[0].contains("Esc"));
}

#[test]
fn list_movement_stops_or_wraps_at_the_ends_and_pages_by_the_given_step() {
    let keys = Keymap::defaults();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    // (key, cursor, len, page, edge, expected)
    let cases = [
        (KeyCode::Up, 0, 5, 2, Edge::Stop, Some(0)),
        (KeyCode::Up, 0, 5, 2, Edge::Wrap, Some(4)),
        (KeyCode::Down, 4, 5, 2, Edge::Stop, Some(4)),
        (KeyCode::Down, 4, 5, 2, Edge::Wrap, Some(0)),
        (KeyCode::PageDown, 1, 5, 2, Edge::Stop, Some(3)),
        (KeyCode::PageDown, 4, 5, 2, Edge::Wrap, Some(4)),
        (KeyCode::PageUp, 1, 5, 2, Edge::Wrap, Some(0)),
        (KeyCode::Char('x'), 1, 5, 2, Edge::Wrap, None),
        // A list with no measured page does not page.
        (KeyCode::PageDown, 1, 5, 0, Edge::Stop, Some(1)),
        (KeyCode::PageUp, 3, 5, 0, Edge::Wrap, Some(3)),
    ];
    for (code, cursor, len, page, edge, expected) in cases {
        assert_eq!(
            list_move(&keys, &key(code), cursor, len, page, edge),
            expected,
            "{code:?} at {cursor} of {len}"
        );
    }
}

#[test]
fn keep_in_view_moves_the_window_only_as_far_as_the_selection_needs() {
    // (selected, scroll, height, total, expected first row)
    let cases = [
        (0, 0, 4, 10, 0),
        (5, 0, 4, 10, 2),
        (2, 4, 4, 10, 2),
        (9, 0, 4, 10, 6),
        (9, 8, 4, 10, 6),
        (1, 3, 4, 3, 0),
    ];
    for (selected, scroll, height, total, expected) in cases {
        assert_eq!(
            crate::ui::chrome::keep_in_view(selected, scroll, height, total),
            expected,
            "selected {selected}, scroll {scroll}, height {height}, total {total}"
        );
    }
}

#[test]
fn an_unbound_key_leaves_no_footer_segment() {
    let theme = Theme::dark();
    assert_eq!(segment("", "move"), "");
    assert_eq!(segment("Esc", ""), "Esc");
    assert_eq!(segment("Enter", "open"), "Enter open");
    let shown = plain(&[super::super::widgets::hint_line(
        &theme,
        &[segment("", "move"), "Esc close".to_owned()],
        40,
    )])
    .concat();
    assert!(
        shown.contains("Esc close") && !shown.contains("move"),
        "{shown}"
    );
}

#[test]
fn help_panels_bound_their_footer_to_the_key_sheet_and_keep_the_way_out() {
    let theme = Theme::dark();
    let hints: Vec<String> = [
        "Enter open",
        "↑/↓ move",
        "a add",
        "k key",
        "x remove",
        "Esc close",
    ]
    .map(String::from)
    .to_vec();
    // (F1 handled by the panel, width, key sheet hint shown, a middle hint shown)
    let cases = [
        (true, 120, true, false),
        (false, 120, false, true),
        (true, 30, false, false),
    ];
    for (help, width, sheet, middle) in cases {
        let line = if help {
            sheet_footer(&theme, hints.clone(), width)
        } else {
            super::super::widgets::hint_line(&theme, &hints, width)
        };
        let shown = line.to_string();
        assert!(
            shown.contains("Esc close"),
            "help {help} width {width}: {shown}"
        );
        assert_eq!(
            shown.contains("F1 keys"),
            sheet,
            "help {help} width {width}: {shown}"
        );
        assert_eq!(
            shown.contains("a add"),
            middle,
            "help {help} width {width}: {shown}"
        );
        assert!(
            text::width(&shown) <= width,
            "help {help} width {width}: {shown}"
        );
    }
}
