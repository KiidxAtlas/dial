use super::*;
fn session(id: &str, title: &str, requests: usize) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        path: format!("/{id}.jsonl").into(),
        started: "2026-01-01T00:00:00Z".into(),
        updated: "2026-01-02T00:00:00Z".into(),
        title: title.into(),
        messages: requests * 2,
        user_messages: requests,
        model: "provider/model".into(),
        parent_id: None,
        first_message: title.into(),
        last_message: "Fix callback retry 日本語".into(),
        last_answer: "Preserve the useful error.".into(),
        project: "project".into(),
    }
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
fn text_of(rows: &Rows) -> String {
    rows.iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn notices_and_selected_sessions_fit_even_the_smallest_browse_layout() {
    let mut picker = Picker::new(
        (0..20)
            .map(|n| session(&format!("id-{n}"), &format!("choice-{n}"), 1))
            .collect(),
        vec![],
        "",
        "",
        false,
    );
    picker.selected = 19;
    picker.notice = Some("This journal is open elsewhere; close it there first.".into());
    for cols in [0, 1, 11, 12, 38, 80, 120] {
        for height in 0..=30 {
            let rows = picker.rows(cols, height, &Theme::dark());
            assert!(rows.len() <= height, "{cols}x{height}: {} rows", rows.len());
            assert!(rows.iter().all(|row| text::line_width(row) <= cols));
            if cols == 0 || height == 0 {
                assert!(rows.is_empty());
            }
            if cols >= 38 && height >= 5 {
                assert!(
                    text_of(&rows).contains("choice-19"),
                    "selection hidden at {cols}x{height}"
                );
                assert!(
                    text_of(&rows).contains("open elsewhere"),
                    "refusal hidden at {cols}x{height}"
                );
            }
        }
    }
}
#[test]
fn conversations_hide_empty_but_pins_and_all_view_keep_them_accessible() {
    let mut p = Picker::new(
        vec![
            session("empty", "", 0),
            session("pin", "Saved empty", 0),
            session("chat", "Fix", 1),
        ],
        vec!["pin".into()],
        "chat",
        "",
        false,
    );
    assert_eq!(p.found, [1, 2]);
    p.handle(&key(KeyCode::Tab, KeyModifiers::NONE), 24);
    assert_eq!(p.found, [0, 1, 2]);
    p.handle(&key(KeyCode::Home, KeyModifiers::NONE), 24);
    assert!(matches!(
        p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
        Action::Resume(0)
    ));
}
#[test]
fn pinning_keeps_selection_and_unpinning_restores_activity_order() {
    let mut older = session("older", "Old", 1);
    older.updated = "2025-01-01T00:00:00Z".into();
    let mut p = Picker::new(
        vec![session("newer", "New", 1), older],
        vec![],
        "",
        "",
        false,
    );
    p.handle(&key(KeyCode::Down, KeyModifiers::NONE), 24);
    assert!(matches!(
        p.handle(&key(KeyCode::Char('p'), KeyModifiers::CONTROL), 24),
        Action::Pin(1)
    ));
    p.set_pinned(1, true);
    assert_eq!(p.sessions[p.found[p.selected]].id, "older");
    assert_eq!(p.found[0], 0);
    p.set_pinned(0, false);
    assert_eq!(p.sessions[0].id, "newer");
    assert_eq!(p.sessions[p.found[p.selected]].id, "older");
}

#[test]
fn tree_includes_empty_sessions_and_never_reorders_ancestry_for_search() {
    let p = Picker::new(
        vec![
            session("parent", "Callback changes", 0),
            session("child", "Callback", 1),
        ],
        vec![],
        "",
        "callback",
        true,
    );
    assert_eq!(p.found, [0, 1]);
}

#[test]
fn tree_orders_children_under_parents_by_recent_activity() {
    let mut root = session("root", "Root", 1);
    root.updated = "2026-01-01T00:00:00Z".into();
    let mut old_child = session("old", "Old child", 1);
    old_child.parent_id = Some("root".into());
    old_child.updated = "2026-01-02T00:00:00Z".into();
    let mut new_child = session("new", "New child", 1);
    new_child.parent_id = Some("root".into());
    new_child.updated = "2026-01-05T00:00:00Z".into();
    let mut other = session("other", "Other", 1);
    other.updated = "2026-02-01T00:00:00Z".into();
    let p = Picker::new(
        vec![root, old_child, new_child, other],
        vec![],
        "",
        "",
        true,
    );
    let ids: Vec<_> = p.found.iter().map(|&i| p.sessions[i].id.as_str()).collect();
    assert_eq!(ids, ["other", "root", "new", "old"]);
    let depths: Vec<_> = p.found.iter().map(|&i| p.depth[i]).collect();
    assert_eq!(depths, [0, 0, 1, 1]);
}

#[test]
fn search_ranks_titles_and_matches_words_in_any_order_without_path_noise() {
    let mut a = session("one", "Callback retry", 1);
    a.last_message = "Other topic".into();
    let p = Picker::new(
        vec![session("two", "Other topic", 1), a],
        vec![],
        "",
        "callback retry",
        false,
    );
    assert_eq!(p.found, [1, 0]);
    let p = Picker::new(p.sessions, vec![], "", "日本語 callback", false);
    assert_eq!(p.found, [0]);
}
#[test]
fn no_matches_is_safe_and_paste_cannot_inject_terminal_controls() {
    let mut p = Picker::new(
        vec![session("one", "Title", 1)],
        vec![],
        "",
        "no-match-123",
        false,
    );
    assert!(matches!(
        p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
        Action::None
    ));
    p.handle(&key(KeyCode::Char('u'), KeyModifiers::CONTROL), 24);
    assert_eq!(p.found, [0]);
    p.handle(&Event::Paste("\x1b[31m日本語\ncallback".into()), 24);
    assert!(!p.query.contains('\x1b'));
    assert!(!p.query.contains('\n'));
}

#[test]
fn escape_clears_the_query_first_and_closes_on_the_second_press() {
    let mut p = Picker::new(vec![session("one", "Title", 1)], vec![], "", "title", false);
    assert!(matches!(
        p.handle(&key(KeyCode::Esc, KeyModifiers::NONE), 24),
        Action::None
    ));
    assert!(p.query.is_empty());
    assert_eq!(p.found, [0]);
    assert!(matches!(
        p.handle(&key(KeyCode::Esc, KeyModifiers::NONE), 24),
        Action::Cancel
    ));
}

#[test]
fn date_group_headers_are_rows_that_navigation_skips() {
    let now = crate::platform::time::parse_utc("2026-03-10T12:00:00").unwrap();
    let mut today = session("today", "Today", 1);
    today.updated = "2026-03-10T11:00:00".into();
    let mut old = session("old", "Old", 1);
    old.updated = "2025-01-01T00:00:00".into();
    let mut p = Picker::new(vec![today, old], vec![], "", "", false);
    assert_eq!(
        p.units(now),
        [
            Unit::Header("Today"),
            Unit::Item(0),
            Unit::Header("Older"),
            Unit::Item(1)
        ]
    );
    p.handle(&key(KeyCode::Down, KeyModifiers::NONE), 24);
    assert_eq!(p.selected_id(), Some("old"));
    let rendered = text_of(&p.rows(80, 24, &Theme::dark()));
    assert!(rendered.contains("Today"));
    assert!(rendered.contains("Older"));
    assert_eq!(day_group("2026-03-09T23:00:00", now), "Yesterday");
    assert_eq!(day_group("2026-03-05T23:00:00", now), "This week");
}

#[test]
fn header_reports_loaded_of_total_and_ctrl_a_loads_only_when_more_exist() {
    let theme = Theme::dark();
    let mut p = Picker::new(
        vec![session("a", "A", 1), session("b", "B", 1)],
        vec![],
        "",
        "",
        false,
    )
    .scope(false, 5);
    assert!(text_of(&p.rows(100, 24, &theme)).contains("showing 2 of 5"));
    assert!(matches!(
        p.handle(&key(KeyCode::Char('a'), KeyModifiers::CONTROL), 24),
        Action::LoadAll
    ));
    let mut full = Picker::new(vec![session("a", "A", 1)], vec![], "", "", false).scope(false, 1);
    assert!(matches!(
        full.handle(&key(KeyCode::Char('a'), KeyModifiers::CONTROL), 24),
        Action::None
    ));
}

#[test]
fn rename_edits_a_draft_in_the_search_line_and_enter_returns_it() {
    let mut p = Picker::new(
        vec![session("one", "Old", 1), session("two", "Other", 1)],
        vec![],
        "",
        "",
        false,
    );
    p.handle(&key(KeyCode::Char('r'), KeyModifiers::CONTROL), 24);
    p.handle(&key(KeyCode::Char('u'), KeyModifiers::CONTROL), 24);
    for c in "New name".chars() {
        p.handle(&key(KeyCode::Char(c), KeyModifiers::NONE), 24);
    }
    match p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24) {
        Action::Rename(index, title) => {
            assert_eq!(p.sessions[index].id, "one");
            assert_eq!(title, "New name");
        }
        _ => panic!("expected a rename"),
    }
}

#[test]
fn wide_layout_previews_the_selected_session_once_loaded() {
    let theme = Theme::dark();
    let mut p = Picker::new(vec![session("one", "Preview me", 1)], vec![], "", "", false);
    assert_eq!(
        p.wanted_preview(110, 38).map(|(id, _)| id).as_deref(),
        Some("one")
    );
    assert!(p.wanted_preview(80, 38).is_none());
    p.set_preview("one".into(), vec![("user".into(), "first question".into())]);
    assert!(p.wanted_preview(110, 38).is_none());
    let rendered = text_of(&p.rows(110, 38, &theme));
    assert!(rendered.contains("You  first question"));
}

#[test]
fn layout_fits_small_and_wide_terminals_without_losing_selection() {
    let theme = Theme::dark();
    let mut p = Picker::new(
        (0..30)
            .map(|i| {
                session(
                    &i.to_string(),
                    "A long Unicode title 日本語 about retrying callbacks",
                    1,
                )
            })
            .collect(),
        vec!["1".into()],
        "2",
        "",
        false,
    );
    for (width, height) in [(110, 38), (80, 24), (44, 18), (30, 12), (12, 5), (8, 3)] {
        p.handle(&key(KeyCode::End, KeyModifiers::NONE), height);
        let rows = p.rows(width, height, &theme);
        assert!(
            rows.len() <= height,
            "{width}x{height}: {} rows",
            rows.len()
        );
        for row in rows {
            let value: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text::width(&value) <= width, "{width}x{height}: {value:?}");
        }
        assert!(matches!(
            p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), height),
            Action::Resume(29)
        ));
    }
}

#[test]
fn sessions_another_process_holds_or_the_current_one_refuse_unsafe_actions_with_a_reason() {
    let mut p = Picker::new(
        vec![session("one", "Busy", 1), session("cur", "Here", 1)],
        vec![],
        "cur",
        "",
        false,
    )
    .with_open_elsewhere(HashSet::from(["one".to_string()]));
    assert!(matches!(
        p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
        Action::None
    ));
    assert!(
        p.notice
            .as_deref()
            .unwrap_or("")
            .contains("another dial process")
    );
    assert!(matches!(
        p.handle(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), 24),
        Action::None
    ));
    assert!(matches!(
        p.handle(&key(KeyCode::Char('r'), KeyModifiers::CONTROL), 24),
        Action::None
    ));
    assert!(p.rename.is_none());
    p.handle(&key(KeyCode::Down, KeyModifiers::NONE), 24);
    assert_eq!(p.selected_id(), Some("cur"));
    assert!(matches!(
        p.handle(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), 24),
        Action::None
    ));
    assert!(p.notice.as_deref().unwrap_or("").contains("/delete"));
    assert!(matches!(
        p.handle(&key(KeyCode::Char('f'), KeyModifiers::CONTROL), 24),
        Action::Fork(1)
    ));
}

#[test]
fn delete_is_offered_only_for_a_session_that_can_be_removed() {
    let mut p = Picker::new(vec![session("one", "Old", 1)], vec![], "other", "", false);
    assert!(matches!(
        p.handle(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), 24),
        Action::Delete(0)
    ));
}

#[test]
fn actions_with_nothing_selected_say_so_instead_of_doing_nothing() {
    let mut p = Picker::new(
        vec![session("one", "Title", 1)],
        vec![],
        "",
        "zzz-none",
        false,
    );
    assert!(matches!(
        p.handle(&key(KeyCode::Enter, KeyModifiers::NONE), 24),
        Action::None
    ));
    assert!(
        p.notice
            .as_deref()
            .unwrap_or("")
            .contains("Nothing selected")
    );
}

#[test]
fn query_edits_keep_the_highlighted_session_while_it_still_matches() {
    let mut p = Picker::new(
        vec![session("a", "Alpha", 1), session("b", "Beta", 1)],
        vec![],
        "",
        "",
        false,
    );
    p.handle(&key(KeyCode::Down, KeyModifiers::NONE), 24);
    assert_eq!(p.selected_id(), Some("b"));
    for c in "be".chars() {
        p.handle(&key(KeyCode::Char(c), KeyModifiers::NONE), 24);
    }
    assert_eq!(p.found, [1]);
    assert_eq!(p.selected_id(), Some("b"));
    p.handle(&key(KeyCode::Esc, KeyModifiers::NONE), 24);
    assert_eq!(p.found.len(), 2);
    assert_eq!(p.selected_id(), Some("b"));
}

#[test]
fn empty_project_points_to_all_projects_and_search_shows_a_hint() {
    let theme = Theme::dark();
    let empty = Picker::new(vec![], vec![], "", "", false).scope(false, 0);
    let rendered = text_of(&empty.rows(80, 24, &theme));
    assert!(
        rendered.contains("Ctrl+G lists every project"),
        "{rendered}"
    );
    let narrow = text_of(&empty.rows(38, 16, &theme));
    assert!(narrow.contains("Ctrl+G projects"), "{narrow}");
    assert!(!narrow.contains("Enter resume"), "{narrow}");
    let p = Picker::new(vec![session("one", "Busy", 1)], vec![], "", "", false)
        .with_open_elsewhere(HashSet::from(["one".to_string()]));
    let rendered = text_of(&p.rows(80, 24, &theme));
    assert!(rendered.contains("type to filter"), "{rendered}");
    assert!(
        rendered.contains("Open in another dial process: resume, rename and delete"),
        "{rendered}"
    );
}
