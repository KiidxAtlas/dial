use super::*;

#[test]
fn row_application_timing_separates_busy_display_restart_and_masked_changes() {
    let rig = Rig::new();
    let active = table(
        "approval = 'ask'\n[ui]\ntips = true\n[coordination]\nenabled = true\n[secrets]\nvalues = ['active-secret-value']",
    );
    let saved = table(
        "approval = 'auto'\n[ui]\ntips = false\n[coordination]\nenabled = false\n[secrets]\nvalues = ['saved-secret-value']",
    );
    let mut panel = SettingsOverlay::new(
        saved.clone(),
        Table::new(),
        saved.clone(),
        Scope::Global,
        vec![],
    );
    panel.set_application_state(&active, true);
    assert_eq!(panel.pending_application_count(), 2);
    for (path, expected) in [
        ("approval", "after this turn"),
        ("ui.tips", "display preference; applies on panel close"),
        ("coordination.enabled", "restart required"),
        ("secrets.values", "Active (hidden) → Saved (hidden)"),
    ] {
        assert!(panel.focus_path(path));
        let shown = draw(&mut panel, &rig, 180, 30);
        assert!(shown.contains(expected), "{path}: {shown}");
        assert!(shown.contains("2 pending"), "{shown}");
        assert!(
            !shown.contains("active-secret-value") && !shown.contains("saved-secret-value"),
            "{shown}"
        );
    }
    assert!(
        panel
            .application_changes
            .values()
            .all(|(a, b)| !a.contains("secret-value") && !b.contains("secret-value"))
    );
    // A new authoritative active snapshot clears pending state; a disk refresh alone does not.
    panel.refresh_values(saved.clone(), Table::new(), saved.clone(), BTreeMap::new());
    panel.set_application_state(&active, true);
    assert_eq!(panel.pending_application_count(), 2);
    panel.set_application_state(&saved, false);
    assert_eq!(panel.pending_application_count(), 0);
    assert!(panel.application_changes.is_empty());
}

#[test]
fn application_snapshot_refresh_keeps_unsaved_editor_and_navigation() {
    let rig = Rig::new();
    let mut panel = overlay();
    assert!(panel.focus_path("ui.status_left"));
    open_text(&mut panel, &rig.keys);
    send(&mut panel, &rig.keys, Event::Paste("unsaved draft".into()));
    let before = (panel.tab, panel.cursor, panel.search.clone());
    let Mode::Edit { editor, .. } = &panel.mode else {
        panic!("editor");
    };
    let draft = editor.text.clone();
    let active = table("approval = 'ask'");
    let saved = table("approval = 'auto'");
    panel.refresh_values(saved.clone(), Table::new(), saved, BTreeMap::new());
    panel.set_application_state(&active, true);
    assert_eq!((panel.tab, panel.cursor, panel.search.clone()), before);
    let Mode::Edit { editor, .. } = &panel.mode else {
        panic!("editor retained");
    };
    assert_eq!(editor.text, draft);
    assert_eq!(panel.pending_application_count(), 1);
}
