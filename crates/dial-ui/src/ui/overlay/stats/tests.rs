use super::*;
use crate::ui::overlay::settings::testkit::{Rig, key, render, screen, status_preview};

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
