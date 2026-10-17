use super::super::settings::testkit::*;
use super::*;
use crossterm::event::KeyCode;

fn view() -> View {
    View {
        enabled: true,
        readiness: "eligible at the next scheduling check".into(),
        state: "idle".into(),
        live_version: Some(3),
        approved: 18,
        needed: 24,
        last_cycle: vec!["self-play · 24 cases".into(), "evaluation · kept v3".into()],
        jobs: vec![
            JobRow {
                id: "abcd".into(),
                label: "project · training · local-model".into(),
                active: true,
                stopping: false,
            },
            JobRow {
                id: "ef01".into(),
                label: "other · idle · local-model".into(),
                active: false,
                stopping: false,
            },
        ],
        versions: vec![
            VersionRow {
                id: 3,
                text: "v3 · active · lora · from v2".into(),
                live: true,
                previous: false,
            },
            VersionRow {
                id: 2,
                text: "v2 · retired · lora · from v1".into(),
                live: false,
                previous: true,
            },
            VersionRow {
                id: 1,
                text: "v1 · retired · base".into(),
                live: false,
                previous: false,
            },
        ],
        ..View::default()
    }
}

fn press(overlay: &mut ImproveOverlay, code: KeyCode) -> OverlayEvent {
    overlay.handle(&key(code), &Keymap::defaults())
}

#[test]
fn tabs_cycle_and_escape_closes_the_panel() {
    let mut overlay = ImproveOverlay::new(view());
    press(&mut overlay, KeyCode::Right);
    assert_eq!(overlay.tab, 1);
    press(&mut overlay, KeyCode::Left);
    press(&mut overlay, KeyCode::Left);
    assert_eq!(overlay.tab, 3);
    assert!(matches!(
        press(&mut overlay, KeyCode::Esc),
        OverlayEvent::Close
    ));
}

#[test]
fn stop_asks_for_enter_and_escape_keeps_the_job() {
    let mut overlay = ImproveOverlay::new(view());
    press(&mut overlay, KeyCode::Right);
    assert!(matches!(
        press(&mut overlay, KeyCode::Enter),
        OverlayEvent::None
    ));
    assert!(overlay.ask.is_some(), "Enter asks first");
    assert!(matches!(
        press(&mut overlay, KeyCode::Esc),
        OverlayEvent::None
    ));
    assert!(
        overlay.ask.is_none(),
        "Esc drops the question without closing"
    );
    press(&mut overlay, KeyCode::Enter);
    let OverlayEvent::Action(action) = press(&mut overlay, KeyCode::Enter) else {
        panic!("confirmed stop sends a command");
    };
    assert_eq!(action, "command:/improve stop abcd");
}

#[test]
fn idle_jobs_have_nothing_to_stop() {
    let mut overlay = ImproveOverlay::new(view());
    press(&mut overlay, KeyCode::Right);
    press(&mut overlay, KeyCode::Down);
    press(&mut overlay, KeyCode::Enter);
    assert!(overlay.ask.is_none());
    assert!(overlay.screen_notice().contains("nothing to stop"));
}

#[test]
fn only_the_previous_version_can_be_rolled_back_after_confirmation() {
    let mut overlay = ImproveOverlay::new(view());
    press(&mut overlay, KeyCode::Right);
    press(&mut overlay, KeyCode::Right);
    press(&mut overlay, KeyCode::Enter);
    assert!(
        overlay.ask.is_none(),
        "the live version is not a rollback target"
    );
    press(&mut overlay, KeyCode::Down);
    press(&mut overlay, KeyCode::Enter);
    assert!(overlay.ask.is_some());
    let OverlayEvent::Action(action) = press(&mut overlay, KeyCode::Enter) else {
        panic!("confirmed rollback sends a command");
    };
    assert_eq!(action, "command:/improve rollback 2 --yes");
    press(&mut overlay, KeyCode::Down);
    press(&mut overlay, KeyCode::Enter);
    assert!(
        overlay.ask.is_none(),
        "an older version cannot be restored from here"
    );
}

#[test]
fn setup_rows_open_the_existing_settings_and_models_views() {
    let mut overlay = ImproveOverlay::new(view());
    for _ in 0..3 {
        press(&mut overlay, KeyCode::Right);
    }
    let OverlayEvent::Action(settings) = press(&mut overlay, KeyCode::Enter) else {
        panic!("settings row opens a view");
    };
    assert_eq!(settings, "open:improve-settings");
    press(&mut overlay, KeyCode::Down);
    let OverlayEvent::Action(models) = press(&mut overlay, KeyCode::Enter) else {
        panic!("models row opens a view");
    };
    assert_eq!(models, "open:improve-models");
}

#[test]
fn status_and_lists_render_plain_lines_without_internal_keys() {
    let rig = Rig::new();
    let mut overlay = ImproveOverlay::new(view());
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let status = screen(&render(100, 24, |frame| overlay.draw(frame, &ctx)));
    assert!(
        status.contains("Approved examples: 18 of 24 needed for training"),
        "{status}"
    );
    assert!(status.contains("Live version: v3"), "{status}");
    assert!(status.contains("self-play · 24 cases"), "{status}");
    assert!(
        !status.contains("[improve]") && !status.contains('{'),
        "{status}"
    );
    press(&mut overlay, KeyCode::Right);
    press(&mut overlay, KeyCode::Right);
    let versions = screen(&render(100, 24, |frame| overlay.draw(frame, &ctx)));
    assert!(
        versions.contains("v2 · retired · lora · from v1"),
        "{versions}"
    );
    let rows = versions
        .lines()
        .filter(|line| line.contains("· retired ·"))
        .count();
    assert_eq!(rows, 2, "one line per version: {versions}");
}

#[test]
fn refresh_is_requested_only_while_a_job_runs() {
    let mut overlay = ImproveOverlay::new(view());
    overlay.refreshed = Instant::now() - REFRESH;
    assert!(overlay.wants_refresh());
    overlay
        .view
        .jobs
        .iter_mut()
        .for_each(|job| job.active = false);
    assert!(!overlay.wants_refresh());
}

#[test]
fn small_panels_render_without_panics() {
    let rig = Rig::new();
    let mut overlay = ImproveOverlay::new(view());
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
