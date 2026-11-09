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
    assert!(content.contains("1 of 2 matches"), "{content}");
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
