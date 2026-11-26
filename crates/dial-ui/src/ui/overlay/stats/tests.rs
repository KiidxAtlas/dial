use super::*;
use crate::ui::overlay::settings::testkit::{Rig, key, render, screen, status_preview};

#[test]
fn the_live_session_tab_leads_and_tab_reaches_the_stored_history() {
    let rig = Rig::new();
    let mut panel = StatsOverlay::new(vec![("Usage / cost", "STORED".into())])
        .with_first_tab("This session", "LIVE $0.12".into());
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let first = screen(&render(80, 20, |f| panel.draw(f, &ctx)));
    assert!(
        first.contains("This session") && first.contains("LIVE $0.12"),
        "{first}"
    );
    panel.handle(&key(KeyCode::Tab), &rig.keys);
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("STORED"));
}

#[test]
fn tabs_search_help_and_escape_preserve_report_navigation() {
    let rig = Rig::new();
    let mut panel = StatsOverlay::new(vec![
        ("Usage", "cost 1\ninput 50".into()),
        ("Tools", "bash 3\nread 2".into()),
    ]);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    panel.handle(&key(KeyCode::Tab), &rig.keys);
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("bash 3"));
    panel.handle(&key(KeyCode::Char('/')), &rig.keys);
    for c in "read".chars() {
        panel.handle(&key(KeyCode::Char(c)), &rig.keys);
    }
    let filtered = screen(&render(80, 20, |f| panel.draw(f, &ctx)));
    assert!(filtered.contains("read 2"));
    assert!(!filtered.contains("bash 3"));
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::None
    ));
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("bash 3"));
    panel.handle(&key(KeyCode::Char('?')), &rig.keys);
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("Stats · keys"));
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::None
    ));
    panel.handle(&key(KeyCode::BackTab), &rig.keys);
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("input 50"));
    assert!(matches!(
        panel.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::Close
    ));
}

#[test]
fn long_reports_scroll_by_wrapped_row_with_an_indicator_and_themed_tabs() {
    let rig = Rig::new();
    let report = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut panel = StatsOverlay::new(vec![("Usage", report), ("Tools", "bash 3".into())]);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let first = screen(&render(60, 16, |f| panel.draw(f, &ctx)));
    assert!(!first.contains("[Usage]"), "tabs use the shared tab style");
    assert!(first.contains("Usage"));
    let thumb = rig.theme.sym(crate::ui::theme::Sym::ScrollThumb);
    assert!(
        first.contains(thumb),
        "an overflowing report shows a scroll thumb"
    );
    assert!(first.contains("Esc close"), "the way out is in the footer");
    panel.handle(&key(KeyCode::End), &rig.keys);
    let last = screen(&render(60, 16, |f| panel.draw(f, &ctx)));
    assert!(last.contains("line 39"));
    assert!(!last.contains("line 0 "));
}

#[test]
fn report_cache_reuses_layout_and_tracks_tab_query_and_width() {
    let mut panel = StatsOverlay::new(vec![
        ("Usage", "中文 👩‍💻 cafe\u{301}\nOther entry\n".into()),
        ("Tools", "read 2".into()),
    ]);
    let initial = panel.report(8).to_vec();
    let allocation = panel.report(8).as_ptr();
    panel.scroll = 1;
    assert_eq!(
        panel.report(8).as_ptr(),
        allocation,
        "scrolling reuses layout"
    );
    assert_eq!(panel.report(8), initial);
    panel.query = "OTHER".into();
    assert_eq!(panel.report(80), ["Other entry"]);
    panel.query.clear();
    assert_eq!(panel.report(80), ["中文 👩‍💻 cafe\u{301}", "Other entry"]);
    assert!(panel.report(3).iter().all(|line| text::width(line) <= 3));
    panel.active = 1;
    assert_eq!(panel.report(80), ["read 2"]);
    panel.query = "missing".into();
    assert!(panel.report(80).is_empty());
    let mut empty = StatsOverlay::new(Vec::new());
    assert!(empty.report(1).is_empty());
}

#[test]
fn mouse_tabs_reuse_report_navigation_and_ignore_help_and_borders() {
    let rig = Rig::new();
    let mut panel = StatsOverlay::new(vec![("Usage", "first".into()), ("Tools", "read 2".into())]);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let buffer = render(80, 20, |f| panel.draw(f, &ctx));
    let text = screen(&buffer);
    let (x, y) = super::super::settings::testkit::position(&text, "Tools");
    let (usage_x, usage_y) = super::super::settings::testkit::position(&text, "Usage");
    panel.scroll = 10;
    panel.searching = true;
    panel.click(x, y);
    assert_eq!((panel.active, panel.scroll, panel.searching), (1, 0, false));
    assert!(screen(&render(80, 20, |f| panel.draw(f, &ctx))).contains("read 2"));
    panel.click(0, 0);
    assert_eq!(panel.active, 1);
    panel.handle(&key(KeyCode::Char('?')), &rig.keys);
    render(80, 20, |f| panel.draw(f, &ctx));
    panel.click(usage_x, usage_y);
    assert_eq!(panel.active, 1);
}
