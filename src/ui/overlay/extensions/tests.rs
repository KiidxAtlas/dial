use super::*;

fn row(id: &str, origin: Origin, state: State) -> Row {
    Row {
        id: id.into(),
        title: id.into(),
        summary: String::new(),
        tags: vec![],
        origin,
        state,
        details: vec![],
    }
}

#[test]
fn arrow_navigation_renders_every_tab_and_wraps() {
    use crate::ui::overlay::settings::testkit::{Rig, key, render, screen, status_preview};
    use crossterm::event::KeyCode;

    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    for (width, height) in [(120, 30), (40, 12), (4, 4)] {
        let mut overlay = ExtensionsOverlay::new();
        for tab in Tab::ALL {
            overlay
                .browser
                .set_rows(tab, vec![row("fixture", Origin::Installed, State::Enabled)]);
        }
        for direction in [KeyCode::Right, KeyCode::Left] {
            for _ in Tab::ALL {
                assert!(matches!(
                    overlay.handle(&key(direction), &rig.keys),
                    OverlayEvent::None
                ));
                let buffer = render(width, height, |frame| overlay.draw(frame, &ctx));
                if width == 120 {
                    assert!(screen(&buffer).contains("fixture"));
                }
            }
            assert_eq!(overlay.browser.tab(), Tab::Mcp);
        }
    }
}

#[test]
fn letter_keys_never_remove_without_confirmation() {
    for tab in Tab::ALL {
        let mut b = Browser::new();
        while b.tab() != tab {
            b.next_tab();
        }
        b.set_rows(tab, vec![row("pkg", Origin::Installed, State::Enabled)]);
        for c in 'a'..='z' {
            let Some(intent) = primary_intent(&b, c) else {
                continue;
            };
            let remove = matches!(
                intent,
                Intent::RemoveMcp { .. } | Intent::RemovePlugin { .. } | Intent::RemoveSkill { .. }
            );
            let ran_now = b.request(intent).is_some();
            assert!(
                !(remove && ran_now),
                "key {c} on {tab:?} removed without confirmation"
            );
            b.decline();
        }
    }
}

#[test]
fn i_imports_another_tools_server_after_confirmation_and_installs_curated_ones_directly() {
    let mut b = Browser::new();
    assert_eq!(b.tab(), Tab::Mcp);
    b.set_rows(
        Tab::Mcp,
        vec![row("codegraph", Origin::Foreign, State::Available)],
    );
    let intent = primary_intent(&b, 'i').expect("i imports a found server");
    assert!(matches!(&intent, Intent::ImportMcp { id } if id == "codegraph"));
    // The import writes Dial's config, so it waits for Enter instead of running at once.
    assert!(b.request(intent).is_none());
    b.decline();
    b.set_rows(
        Tab::Mcp,
        vec![row("github", Origin::Curated, State::Available)],
    );
    assert!(matches!(
        primary_intent(&b, 'i'),
        Some(Intent::InstallCuratedMcp { id }) if id == "github"
    ));
    // An already-installed row is never offered the import.
    b.set_rows(
        Tab::Mcp,
        vec![row("codegraph", Origin::Installed, State::Enabled)],
    );
    assert!(!matches!(
        primary_intent(&b, 'i'),
        Some(Intent::ImportMcp { .. })
    ));
}

#[test]
fn remove_waits_for_enter_and_esc_cancels() {
    let mut b = Browser::new();
    b.set_rows(
        Tab::Mcp,
        vec![row("memory", Origin::Installed, State::Enabled)],
    );
    let intent = primary_intent(&b, 'x').expect("x removes the selected server");
    assert!(b.request(intent).is_none());
    assert!(b.confirm.is_some());
    b.decline();
    assert!(b.confirm.is_none());
}

#[test]
fn enter_installs_available_rows_and_shows_details_for_installed_ones() {
    let mut b = Browser::new();
    b.set_rows(
        Tab::Plugins,
        vec![
            row("alpha@market", Origin::Remote, State::Available),
            row("beta", Origin::Installed, State::Enabled),
        ],
    );
    b.next_tab();
    assert_eq!(
        enter_install(&b),
        Some(Intent::InstallPlugin {
            id: "alpha@market".into()
        })
    );
    assert_eq!(enter_label(&b), "install");
    b.move_cursor(1);
    assert!(enter_install(&b).is_none());
    assert_eq!(enter_label(&b), "details");
}

#[test]
fn scrolling_near_the_end_loads_the_next_catalog_page_once() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;

    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    let rows = (0..8)
        .map(|i| row(&format!("srv{i}"), Origin::Remote, State::Available))
        .collect();
    overlay.browser.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: String::new(),
        page: 1,
        rows,
        more: true,
        error: None,
        message: "page 1".into(),
    });
    // Far from the end: nothing is requested.
    assert!(matches!(
        overlay.handle(&key(KeyCode::Down), &rig.keys),
        OverlayEvent::None
    ));
    // Within the prefetch distance: page 2 is requested.
    overlay.browser.move_cursor(4);
    let OverlayEvent::Action(request) = overlay.handle(&key(KeyCode::Down), &rig.keys) else {
        panic!("expected a page request");
    };
    assert!(request.contains("\"page\":2"), "{request}");
    // While that page loads, further scrolling does not request it again.
    assert!(matches!(
        overlay.handle(&key(KeyCode::Down), &rig.keys),
        OverlayEvent::None
    ));
}

#[test]
fn a_failed_page_stops_automatic_paging() {
    let mut b = Browser::new();
    b.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: String::new(),
        page: 2,
        rows: Vec::new(),
        more: false,
        error: Some("offline".into()),
        message: String::new(),
    });
    assert!(!b.catalog(Tab::Mcp).more);
}
