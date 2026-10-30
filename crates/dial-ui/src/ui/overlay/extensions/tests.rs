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
                Intent::RemoveMcp { .. } | Intent::RemoveSkill { .. }
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
        Tab::Skills,
        vec![
            row("alpha@market", Origin::Remote, State::Available),
            row("beta", Origin::Installed, State::Enabled),
        ],
    );
    b.next_tab();
    assert_eq!(
        enter_install(&b),
        Some(Intent::InstallSkill {
            source: "alpha@market".into(),
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
fn a_query_missing_from_the_loaded_page_offers_and_runs_a_catalog_search() {
    use crate::ui::overlay::settings::testkit::{Rig, key, render, screen, status_preview};
    use crossterm::event::KeyCode;

    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut overlay = ExtensionsOverlay::new();
    overlay.browser.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: String::new(),
        page: 1,
        rows: vec![row("memory", Origin::Remote, State::Available)],
        more: true,
        error: None,
        message: "page 1".into(),
    });
    overlay.handle(&key(KeyCode::Char('/')), &rig.keys);
    for c in "supabase".chars() {
        overlay.handle(&key(KeyCode::Char(c)), &rig.keys);
    }
    let shown = screen(&render(200, 20, |frame| overlay.draw(frame, &ctx)));
    assert!(
        shown.contains("searching every MCP catalog when you pause"),
        "{shown}"
    );
    assert!(
        shown.contains("searched when you pause typing, or press Enter"),
        "{shown}"
    );

    let OverlayEvent::Action(request) = overlay.handle(&key(KeyCode::Enter), &rig.keys) else {
        panic!("Enter in search must query the catalogs");
    };
    assert!(request.contains("\"query\":\"supabase\""), "{request}");
    // Enter already searched, so the typing pause does not search again.
    let later = std::time::Instant::now() + ExtensionsOverlay::SEARCH_PAUSE * 4;
    assert_eq!(overlay.catalog_search_due(later), None);

    // Leaving search with the filter kept: Enter on the empty list still searches the catalogs.
    let mut overlay = ExtensionsOverlay::new();
    overlay.browser.start_search();
    for c in "supabase".chars() {
        overlay.browser.push_query(c);
    }
    overlay.browser.end_search();
    let OverlayEvent::Action(request) = overlay.handle(&key(KeyCode::Enter), &rig.keys) else {
        panic!("Enter on an empty filtered list must query the catalogs");
    };
    assert!(request.contains("\"query\":\"supabase\""), "{request}");
    assert!(!overlay.browser.catalog_search_pending());
}

#[test]
fn a_pause_in_typing_searches_the_mcp_catalogs_without_enter() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;
    use std::time::Instant;

    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    overlay.browser.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: String::new(),
        page: 1,
        rows: vec![row("memory", Origin::Remote, State::Available)],
        more: true,
        error: None,
        message: "page 1".into(),
    });
    overlay.handle(&key(KeyCode::Char('/')), &rig.keys);
    for c in "supabase".chars() {
        overlay.handle(&key(KeyCode::Char(c)), &rig.keys);
    }
    // Still typing: nothing is sent yet.
    assert_eq!(overlay.catalog_search_due(Instant::now()), None);
    let paused = Instant::now() + ExtensionsOverlay::SEARCH_PAUSE;
    assert_eq!(
        overlay.catalog_search_due(paused).as_deref(),
        Some("supabase")
    );
    // Sent once per edit.
    assert_eq!(overlay.catalog_search_due(paused), None);

    // The background search shows its rows under the query, as Enter would.
    let listing = SharedCatalog::default();
    overlay.attach_catalog(Tab::Mcp, "supabase".into(), listing.clone());
    assert!(overlay.browser.catalog(Tab::Mcp).loading);
    *listing.lock().unwrap() = Some(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: "supabase".into(),
        page: 1,
        rows: vec![row("com.supabase/mcp", Origin::Remote, State::Available)],
        more: false,
        error: None,
        message: "page 1".into(),
    });
    overlay.tick();
    let visible: Vec<_> = overlay
        .browser
        .visible(Tab::Mcp)
        .iter()
        .map(|row| row.id.clone())
        .collect();
    assert_eq!(visible, ["com.supabase/mcp"]);

    // An edit made while a search is loading is searched once that search lands.
    overlay.handle(&key(KeyCode::Backspace), &rig.keys);
    let listing = SharedCatalog::default();
    overlay.attach_catalog(Tab::Mcp, "supabase".into(), listing.clone());
    let paused = Instant::now() + ExtensionsOverlay::SEARCH_PAUSE;
    assert_eq!(overlay.catalog_search_due(paused), None);
    *listing.lock().unwrap() = Some(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: "supabase".into(),
        page: 1,
        rows: Vec::new(),
        more: false,
        error: None,
        message: "page 1".into(),
    });
    overlay.tick();
    assert_eq!(
        overlay.catalog_search_due(paused).as_deref(),
        Some("supabas")
    );
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

#[test]
fn empty_tabs_wrap_their_way_forward_and_hints_skip_unavailable_actions() {
    use crate::ui::overlay::settings::testkit::{Rig, render, rows, status_preview};
    let ascii = Rig::with_theme(
        crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
    );
    let ctx = DrawCtx {
        theme: &ascii.theme,
        keys: &ascii.keys,
        status: &ascii.status,
        preview_status: &status_preview,
    };
    let mut overlay = ExtensionsOverlay::new();
    let screen = rows(&render(38, 16, |f| overlay.draw(f, &ctx))).join("\n");
    let flat = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    // The whole empty message is readable, never clipped at the edge.
    assert!(flat.contains("search the catalog."), "{screen}");
    // Nothing is selected and no next page is pending: no row actions or paging are offered.
    for absent in [
        "t test",
        "x remove",
        "loads more",
        "Enter details",
        "0 shown",
    ] {
        assert!(!screen.contains(absent), "{absent}: {screen}");
    }
    // An installed MCP server offers test, disable and remove, which its keys really do.
    overlay
        .browser
        .set_rows(Tab::Mcp, vec![row("fs", Origin::Installed, State::Enabled)]);
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let screen = rows(&render(160, 20, |f| overlay.draw(f, &ctx))).join("\n");
    assert!(
        screen.contains("t test") && screen.contains("e disable") && screen.contains("x remove"),
        "{screen}"
    );
}

#[test]
fn search_navigation_keeps_filter_and_selects_results_without_installing() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;
    let rig = Rig::new();
    for (navigation, expected) in [
        (KeyCode::Down, 1),
        (KeyCode::PageDown, 8),
        (KeyCode::End, 11),
        (KeyCode::Up, 0),
        (KeyCode::Home, 0),
        (KeyCode::PageUp, 0),
    ] {
        let mut overlay = ExtensionsOverlay::new();
        overlay.browser.set_rows(
            Tab::Mcp,
            (0..12)
                .map(|n| row(&format!("server-{n}"), Origin::Remote, State::Available))
                .collect(),
        );
        overlay.handle(&key(KeyCode::Char('/')), &rig.keys);
        for c in "server".chars() {
            overlay.handle(&key(KeyCode::Char(c)), &rig.keys);
        }
        assert!(matches!(
            overlay.handle(&key(navigation), &rig.keys),
            OverlayEvent::None
        ));
        assert_eq!(overlay.browser.cursor(), expected);
        assert_eq!(overlay.browser.query(), "server");
        assert!(!overlay.browser.searching());
        assert!(overlay.browser.confirm.is_none());
        overlay.handle(&key(KeyCode::Enter), &rig.keys);
        assert!(
            matches!(&overlay.browser.confirm, Some(Intent::InstallMcp { id, .. }) if id == &format!("server-{expected}"))
        );
    }
}

#[test]
fn search_keys_do_not_run_actions_or_repeat_completed_queries() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;
    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    overlay.browser.start_search();
    for c in "jkrx".chars() {
        overlay.handle(&key(KeyCode::Char(c)), &rig.keys);
    }
    assert_eq!(overlay.browser.query(), "jkrx");
    assert!(overlay.browser.confirm.is_none());
    overlay.browser.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: "jkrx".into(),
        page: 1,
        rows: vec![row("jkrx", Origin::Remote, State::Available)],
        more: false,
        error: None,
        message: String::new(),
    });
    assert!(matches!(
        overlay.handle(&key(KeyCode::Enter), &rig.keys),
        OverlayEvent::None
    ));
    assert!(!overlay.browser.searching());
    assert!(overlay.browser.confirm.is_none());
    overlay.browser.start_search();
    overlay.handle(&key(KeyCode::Right), &rig.keys);
    assert_eq!(overlay.browser.tab(), Tab::Skills);
    assert_eq!(overlay.browser.query(), "jkrx");
}

#[test]
fn search_navigation_does_not_page_the_previous_query() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;
    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    overlay.browser.apply_catalog(model::CatalogUpdate {
        tab: Tab::Mcp,
        query: String::new(),
        page: 1,
        rows: vec![row("server", Origin::Remote, State::Available)],
        more: true,
        error: None,
        message: String::new(),
    });
    overlay.handle(&key(KeyCode::Char('/')), &rig.keys);
    overlay.handle(&key(KeyCode::Char('s')), &rig.keys);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Down), &rig.keys),
        OverlayEvent::None
    ));
    assert_eq!(
        overlay
            .catalog_search_due(Instant::now() + ExtensionsOverlay::SEARCH_PAUSE)
            .as_deref(),
        Some("s")
    );
}

#[test]
fn result_tags_are_visible_before_long_descriptions() {
    use crate::ui::overlay::settings::testkit::{Rig, render, screen, status_preview};
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let mut overlay = ExtensionsOverlay::new();
    let mut entry = row("service", Origin::Remote, State::Available);
    entry.tags = vec!["registry".into(), "API key required".into()];
    entry.summary = "long description ".repeat(30);
    overlay.browser.set_rows(Tab::Mcp, vec![entry]);
    let shown = screen(&render(100, 20, |f| overlay.draw(f, &ctx)));
    assert!(shown.contains("API key required"), "{shown}");
    overlay.browser.start_search();
    let shown = screen(&render(100, 20, |f| overlay.draw(f, &ctx)));
    assert!(shown.contains("browse results"), "{shown}");
    assert!(!shown.contains("Enter install"), "{shown}");
}

#[test]
fn paste_edits_only_the_focused_field_without_submitting() {
    use crate::ui::overlay::settings::testkit::Rig;
    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    let paste = Event::Paste("/exit\r\n\t秘密".into());
    assert!(matches!(
        overlay.handle(&paste, &rig.keys),
        OverlayEvent::None
    ));
    assert_eq!(overlay.browser.query(), "");
    overlay.browser.start_search();
    assert!(matches!(
        overlay.handle(&paste, &rig.keys),
        OverlayEvent::None
    ));
    assert_eq!(overlay.browser.query(), "/exit秘密");
    assert_eq!(
        overlay.catalog_search_due(Instant::now() + ExtensionsOverlay::SEARCH_PAUSE),
        Some("/exit秘密".into())
    );
    overlay.browser.form = Some(Form::key("fixture", None));
    assert!(matches!(
        overlay.handle(&Event::Paste("test-token\r\n".into()), &rig.keys),
        OverlayEvent::None
    ));
    let form = overlay.browser.form.as_ref().unwrap();
    assert_eq!(form.fields[0].value, "test-token");
    assert!(form.fields[0].secret);
    assert_eq!(overlay.browser.query(), "/exit秘密");
    assert!(overlay.browser.confirm.is_none());
    overlay.browser.form = None;
    overlay.browser.request(Intent::RemoveMcp {
        id: "fixture".into(),
    });
    assert!(matches!(
        overlay.handle(&Event::Paste("yes\n".into()), &rig.keys),
        OverlayEvent::None
    ));
    assert!(overlay.browser.confirm.is_some());
    assert_eq!(overlay.browser.query(), "/exit秘密");
}

#[test]
fn rejected_catalog_retains_earlier_form_and_completion_and_allows_retry() {
    use crate::ui::overlay::settings::testkit::{Rig, key};
    use crossterm::event::KeyCode;
    let rig = Rig::new();
    let mut overlay = ExtensionsOverlay::new();
    let install = SharedProgress::default();
    install.lock().unwrap().label = Some("install".into());
    overlay.attach(install.clone());
    assert!(matches!(
        overlay.browse(Tab::Mcp, "fixture".into(), 1),
        OverlayEvent::Action(_)
    ));
    let rejected = SharedProgress::default();
    *rejected.lock().unwrap() = JobProgress {
        result: Some((false, "busy".into())),
        catalog: Some(model::CatalogUpdate {
            tab: Tab::Mcp,
            query: "fixture".into(),
            page: 1,
            rows: vec![],
            more: false,
            error: Some("busy".into()),
            message: String::new(),
        }),
        ..Default::default()
    };
    overlay.attach(rejected);
    overlay.tick();
    assert!(!overlay.browser.catalog(Tab::Mcp).loading);
    assert!(!overlay.take_refresh());
    assert_eq!(overlay.progress.len(), 1);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Char('r')), &rig.keys),
        OverlayEvent::Action(_)
    ));
    {
        let mut progress = install.lock().unwrap();
        progress.form = Some(Form::key("fixture", None));
        progress.result = Some((true, "ready for input".into()));
    }
    overlay.tick();
    assert!(overlay.browser.form.is_some());
    assert!(overlay.take_refresh());
    assert!(overlay.progress.is_empty());
    overlay.browser.form = None;
    overlay.tick();
    assert!(overlay.browser.form.is_none());
    assert!(!overlay.take_refresh());
}

#[test]
fn errors_from_the_controller_show_as_a_failed_job_and_notices_stay_neutral() {
    let mut overlay = ExtensionsOverlay::new();
    overlay.banner("compute refused", true);
    assert!(matches!(
        &overlay.browser.job,
        Job::Done { ok: false, message, .. } if message == "compute refused"
    ));
    overlay.banner("Applying", false);
    assert!(matches!(overlay.browser.job, Job::Idle));
    assert_eq!(overlay.browser.message, "Applying");
}
