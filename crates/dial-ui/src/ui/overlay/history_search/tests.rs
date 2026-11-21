use super::super::settings::testkit::*;
use super::*;

fn hit(session: &str, entry: &str) -> SearchHit {
    SearchHit {
        session_id: session.into(),
        entry_id: entry.into(),
        role: "user".into(),
        snippet: "needle".into(),
        preview: "before needle after".into(),
        when: "today".into(),
    }
}

#[test]
fn selection_emits_resume_only_and_cancel_never_emits_navigation() {
    let keys = Keymap::defaults();
    let mut panel = HistorySearchOverlay::new(
        "needle".into(),
        vec![hit("one", "entry-one"), hit("two", "entry-two")],
        true,
    );
    assert!(matches!(
        panel.handle(&key(KeyCode::Down), &keys),
        OverlayEvent::None
    ));
    assert!(
        matches!(panel.handle(&key(KeyCode::Enter), &keys), OverlayEvent::Action(value) if value == "history-open:two")
    );
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
    assert!(
        matches!(panel.handle(&ch('b'), &keys), OverlayEvent::Action(value) if value == "history-return")
    );
    let mut empty = HistorySearchOverlay::new("none".into(), vec![], false);
    for event in [key(KeyCode::Enter), ch('b'), key(KeyCode::Down)] {
        assert!(matches!(empty.handle(&event, &keys), OverlayEvent::None));
    }
}

#[test]
fn preview_displays_stable_identity_and_context_at_bounded_sizes() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut panel =
        HistorySearchOverlay::new("needle".into(), vec![hit("session", "entry-id")], false);
    let buffer = render(100, 24, |frame| panel.draw(frame, &ctx));
    let content = screen(&buffer);
    assert!(content.contains("entry-id"), "{content}");
    assert!(content.contains("before needle after"), "{content}");
    for (w, h) in [(1, 1), (10, 3), (40, 10)] {
        render(w, h, |frame| panel.draw(frame, &ctx));
    }
}

#[test]
fn compact_audit_history_page_keys_do_not_skip_wrapped_rows_after_resize() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let words: Vec<_> = (0..60).map(|i| format!("paragraph-line-{i:05}")).collect();
    let mut item = hit("one", "first");
    item.preview = words.join(" ");
    let mut panel = HistorySearchOverlay::new("needle".into(), vec![item], false);
    for height in [10, 16] {
        let first = screen(&render(38, height, |f| panel.draw(f, &ctx)));
        let count = words
            .iter()
            .take_while(|w| first.contains(w.as_str()))
            .count();
        assert!(count > 0 && count < words.len());
        panel.handle(&key(KeyCode::PageDown), &rig.keys);
        let next = screen(&render(38, height, |f| panel.draw(f, &ctx)));
        assert!(
            !next.contains(&words[count - 1]) && next.contains(&words[count]),
            "{next}"
        );
        panel.handle(&key(KeyCode::PageUp), &rig.keys);
        assert_eq!(screen(&render(38, height, |f| panel.draw(f, &ctx))), first);
    }
}

#[test]
fn narrow_history_keeps_open_close_and_return_controls_visible() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut panel = HistorySearchOverlay::new("needle".into(), vec![hit("session", "entry")], true);
    let content = screen(&render(38, 16, |frame| panel.draw(frame, &ctx)));
    for control in ["Enter open", "Esc close", "b return", "PgUp/PgDn preview"] {
        assert!(content.contains(control), "{control}: {content}");
    }
}

#[test]
fn slash_filters_the_hits_titles_replace_ids_and_no_fixed_branch_note() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut other = hit("session-two", "entry-two");
    other.snippet = "haystack".into();
    let titles = [("session-one".to_string(), "Fix the login flow".to_string())]
        .into_iter()
        .collect();
    let mut panel = HistorySearchOverlay::new(
        "needle".into(),
        vec![hit("session-one", "entry-one"), other],
        true,
    )
    .with_titles(titles);
    let content = screen(&render(100, 24, |frame| panel.draw(frame, &ctx)));
    assert!(content.contains("Fix the login flow"), "{content}");
    assert!(!content.contains("no branch changes"), "{content}");
    assert!(
        !content.contains("> "),
        "the selection uses the shared row, not `>`"
    );
    // `/` starts the filter; `b` is then typed instead of returning.
    panel.handle(&ch('/'), &rig.keys);
    for c in "hay".chars() {
        assert!(matches!(
            panel.handle(&ch(c), &rig.keys),
            OverlayEvent::None
        ));
    }
    assert!(matches!(
        panel.handle(&ch('b'), &rig.keys),
        OverlayEvent::None
    ));
    panel.handle(&key(KeyCode::Backspace), &rig.keys);
    let content = screen(&render(100, 24, |frame| panel.draw(frame, &ctx)));
    assert!(content.contains("1 of 2 loaded"), "{content}");
    panel.handle(&key(KeyCode::Enter), &rig.keys);
    assert!(
        matches!(panel.handle(&key(KeyCode::Enter), &rig.keys), OverlayEvent::Action(v) if v == "history-open:session-two")
    );
    // Esc clears the filter first, then closes.
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::Close
    ));
}

/// Backspace in the filter removes a whole accented letter, not just its combining mark.
#[test]
fn filter_backspace_removes_a_whole_grapheme() {
    let keys = Keymap::defaults();
    let mut panel = HistorySearchOverlay::new("needle".into(), vec![hit("one", "entry")], false);
    panel.handle(&ch('/'), &keys);
    for c in ['n', 'e', '\u{301}'] {
        panel.handle(&ch(c), &keys);
    }
    assert_eq!(panel.filter, "ne\u{301}");
    panel.handle(&key(KeyCode::Backspace), &keys);
    assert_eq!(panel.filter, "n");
}

#[test]
fn result_count_describes_loaded_window_not_global_total() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut panel = HistorySearchOverlay::new("needle".into(), vec![hit("one", "entry")], false);
    let content = screen(&render(100, 24, |frame| panel.draw(frame, &ctx)));
    assert!(content.contains("1 loaded"), "{content}");
}

#[test]
fn more_action_only_exists_for_expandable_windows_and_is_text_while_filtering() {
    let keys = Keymap::defaults();
    for (has_more, can_load, emits) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
    ] {
        let mut panel =
            HistorySearchOverlay::new("needle".into(), vec![], false).with_more(has_more, can_load);
        assert_eq!(
            matches!(panel.handle(&ch('m'), &keys), OverlayEvent::Action(v) if v == "history-more"),
            emits
        );
        panel.handle(&ch('/'), &keys);
        assert!(matches!(panel.handle(&ch('m'), &keys), OverlayEvent::None));
        assert_eq!(panel.filter, "m");
    }
}
