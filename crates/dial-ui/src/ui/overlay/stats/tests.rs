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
