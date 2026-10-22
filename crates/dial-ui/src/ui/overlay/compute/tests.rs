use super::super::settings::testkit::*;
use super::*;
use crossterm::event::KeyCode;
fn fixture() -> Value {
    json!({"busy":true,"owner":{"kind":"background","pid":42,"model":"local-model"},
    "jobs":[{"state":"/job","model":"local","project":"/project","active":true,"progress":{"stage":"training"},"training":{"phase":"sft","iteration":4,"total":32,"peak_memory_gb":1.25}}],
    "processes":[{"pid":70,"identity":"model70","kind":"models","model":"big-model","phase":"loading","memory_bytes":36507222016_u64,"cpu_percent":80.0},
    {"pid":71,"identity":"service71","kind":"services","model":"Laya","phase":"running"}, {"pid":72,"identity":"tool72","kind":"tools","model":"python","phase":"running"}]})
}
#[test]
fn tabs_filter_sections_and_x_targets_the_selected_process() {
    let keys = Keymap::defaults();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    assert_eq!(overlay.items().len(), 7);
    overlay.handle(&key(KeyCode::Right), &keys);
    assert_eq!(overlay.tab, 1);
    assert_eq!(overlay.items().len(), 1);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::None
    ));
    let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
        panic!("missing stop");
    };
    let payload: Value = serde_json::from_str(action.strip_prefix("compute:").unwrap()).unwrap();
    assert_eq!(payload["action"], "stop_process");
    assert_eq!(payload["pid"], 70);
    assert_eq!(payload["identity"], "model70");
    overlay.handle(&key(KeyCode::Right), &keys);
    assert_eq!(overlay.items().len(), 4);
    overlay.cursor = 3;
    assert!(matches!(
        overlay.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::None
    ));
    let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
        panic!("missing remove");
    };
    assert!(action.contains("remove") && action.contains("/job"));
    overlay.handle(&key(KeyCode::Right), &keys);
    assert_eq!(overlay.items()[0].section, "Services");
    overlay.handle(&key(KeyCode::Right), &keys);
    assert_eq!(overlay.items()[0].section, "Tools & subprocesses");
    overlay.handle(&key(KeyCode::Right), &keys);
    assert_eq!(overlay.tab, 0);
}
#[test]
fn menu_keeps_identity_when_the_inventory_changes() {
    let keys = Keymap::defaults();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    overlay.tab = 2;
    overlay.cursor = 3;
    overlay.handle(&key(KeyCode::Enter), &keys);
    overlay.cursor = 1;
    overlay.update(json!({"jobs":[{"state":"/different/job","model":"changed"}]}));
    let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
        panic!("missing stop");
    };
    assert!(
        action.contains("stop") && action.contains("/job") && !action.contains("/different/job")
    );
}
#[test]
fn refresh_keeps_process_selection_and_repeated_x_does_not_stop_another_item() {
    let keys = Keymap::defaults();
    let mut overlay = ComputeOverlay::new();
    let mut status = fixture();
    status["processes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"pid":80,"identity":"model80","kind":"models","model":"second"}));
    overlay.update(status.clone());
    overlay.tab = 1;
    overlay.cursor = 1;
    status["processes"].as_array_mut().unwrap().reverse();
    overlay.update(status);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::None
    ));
    let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
        panic!("missing stop");
    };
    assert!(action.contains("model80"));
    let repeat = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
        crossterm::event::KeyEventKind::Repeat,
    ));
    assert!(matches!(overlay.handle(&repeat, &keys), OverlayEvent::None));
}
#[test]
fn active_rows_are_green_even_when_selected_and_show_large_memory() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    overlay.cursor = 3;
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let buffer = render(150, 35, |frame| overlay.draw(frame, &ctx));
    let shown_rows = rows(&buffer);
    let shown = shown_rows.join("\n");
    for expected in [
        "All",
        "Models",
        "Background",
        "Services",
        "Tools",
        "36.51 GB",
        "loading",
        "Owner: background",
        "x stop",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
    let y = shown_rows
        .iter()
        .position(|row| row.contains("big-model"))
        .unwrap() as u16;
    // Use the label cell rather than its byte offset (the row contains Unicode glyphs).
    let cell = (0..buffer.area.width)
        .find(|&x| buffer[(x, y)].symbol() == "b")
        .unwrap();
    assert_eq!(buffer[(cell, y)].fg, rig.theme.fg(Tok::Success).fg.unwrap());
}
#[test]
fn memory_totals_include_all_process_footprint_once_in_every_tab() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(json!({"session_memory_bytes":100_000_000_u64,
        "processes":[
            {"pid":70,"identity":"model70","kind":"models","model":"model",
                "memory_bytes":2_000_000_000_u64,"memory_metric":"footprint"},
            {"pid":71,"identity":"laya71","kind":"services","model":"Laya",
                "memory_bytes":38_500_000_000_u64,
                "resident_memory_bytes":150_000_000_u64,"memory_metric":"footprint"}
        ],"jobs":[{"state":"/job","model":"model","active":true}]}));
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    for tab in 0..TABS.len() {
        overlay.tab = tab;
        let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
        assert!(shown.contains("Memory: Dial 40.60 GB"), "{shown}");
        assert!(shown.contains("compressed/GPU allocations"));
    }
    overlay.tab = 3;
    overlay.handle(&key(KeyCode::Enter), &rig.keys);
    let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
    assert!(shown.contains("Memory footprint: 38.50 GB · Resident (RSS): 0.150 GB"));
}
#[test]
fn selected_job_shows_training_details() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    overlay.menu = Some(Target::Job("/job".into()));
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let shown = rows(&render(120, 30, |frame| overlay.draw(frame, &ctx))).join("\n");
    assert!(shown.contains("Training: sft 4/32"));
    assert!(shown.contains("Peak MLX memory: 1.25 GB"));
    assert!(shown.contains("Stop job now"));
}
#[test]
fn small_panels_render_without_panics() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    for width in [1, 12, 40, 120] {
        for height in [1, 4, 12, 30] {
            render(width, height, |frame| overlay.draw(frame, &ctx));
        }
    }
}

#[test]
fn esc_from_an_item_menu_restores_the_list_cursor_and_stop_now_asks_first() {
    let keys = Keymap::defaults();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    overlay.tab = 2;
    overlay.cursor = 3;
    overlay.handle(&key(KeyCode::Enter), &keys);
    overlay.handle(&key(KeyCode::Esc), &keys);
    assert_eq!(
        overlay.cursor, 3,
        "Esc returns to the item it was opened from"
    );
    overlay.handle(&key(KeyCode::Enter), &keys);
    overlay.handle(&key(KeyCode::Down), &keys);
    overlay.handle(&key(KeyCode::Down), &keys);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert!(overlay.confirm.is_some(), "stop now waits for Enter");
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(overlay.confirm.is_none(), "Esc keeps the job running");
}

#[test]
fn banners_carry_success_or_error_tone() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    overlay.banner("stop failed".into(), true);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let buffer = render(100, 30, |frame| overlay.draw(frame, &ctx));
    assert_eq!(
        style_of(&buffer, "stop failed").and_then(|style| style.fg),
        rig.theme.fg(Tok::Error).fg
    );
}

#[test]
fn home_end_and_page_keys_move_the_cursor_within_the_list() {
    let keys = Keymap::defaults();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    let last = overlay.items().len() - 1;
    overlay.handle(&key(KeyCode::End), &keys);
    assert_eq!(overlay.cursor, last);
    overlay.handle(&key(KeyCode::Home), &keys);
    assert_eq!(overlay.cursor, 0);
    overlay.handle(&key(KeyCode::PageDown), &keys);
    assert_eq!(
        overlay.cursor, last,
        "a page past the end stops at the last row"
    );
    overlay.handle(&key(KeyCode::PageUp), &keys);
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn hints_name_what_x_and_enter_do_on_the_selected_row() {
    let rig = Rig::new();
    let mut overlay = ComputeOverlay::new();
    overlay.update(fixture());
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    // A control row: Enter runs it and x does nothing, so x is not offered.
    let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
    assert!(shown.contains("Enter run"), "{shown}");
    assert!(
        !shown.contains("x stop") && !shown.contains("x remove"),
        "{shown}"
    );
    overlay.tab = 2;
    overlay.cursor = 3;
    let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
    assert!(shown.contains("x remove"), "{shown}");
}
