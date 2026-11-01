use super::*;
use crate::config::types::Config;
use crate::ui::status::StatusData;
use crate::ui::welcome::{Home, Pick, WelcomeInfo};

#[test]
fn idle_status_refresh_keeps_last_request_settings_in_plan_mode() {
    let config = Config {
        model: "model".into(),
        ..Default::default()
    };
    let mut status = StatusData::default();
    let mut home = None;
    update_model_status(&mut status, &mut home, &config);
    status.model.push_str(" [plan]");
    status.inference = "last request controls".into();
    update_model_status(&mut status, &mut home, &config);
    assert_eq!(status.inference, "last request controls");
}

#[test]
fn model_change_updates_status_and_home_without_resetting_navigation() {
    let mut status = StatusData::default();
    let mut home = Some(Home::new(
        WelcomeInfo {
            recent: vec![("existing session".into(), "1h".into())],
            ..Default::default()
        },
        vec!["session-id".into()],
        false,
    ));
    home.as_mut().unwrap().down();
    for model in ["first-model", "replacement-model"] {
        let config = Config {
            model: model.into(),
            ..Default::default()
        };
        status.inference = "previous model settings".into();
        update_model_status(&mut status, &mut home, &config);
        assert_eq!(status.model, model);
        assert!(
            status.inference.is_empty(),
            "A model switch clears old request settings"
        );
        let home = home.as_ref().unwrap();
        assert_eq!(home.info.model, status.model);
        assert_eq!(home.info.provider, status.provider);
        assert_eq!(home.selected(), Some(0));
    }
    assert_eq!(
        home.as_mut().unwrap().pick(),
        Some(Pick::Resume("session-id".into()))
    );
    home = None;
    update_model_status(&mut status, &mut home, &Config::default());
    assert!(
        home.is_none(),
        "A conversation must not reopen the welcome screen"
    );
}

#[test]
fn a_command_report_on_the_home_screen_ends_the_home_so_it_is_visible() {
    let (_home, _project, _app, mut shell) = super::super::testkit::shell();
    shell.home = Some(Home::new(WelcomeInfo::default(), Vec::new(), false));
    // Background notices at launch keep the home: the user has not asked for anything yet.
    shell.note("Workers", "restored");
    shell.paint(None).unwrap();
    assert!(shell.home.is_some());

    // A submitted command that only opens a panel reports nothing, so the home stays.
    shell.mark_home_input();
    shell.paint(None).unwrap();
    assert!(shell.home.is_some());

    // Its report (here `/add-dir`) would land in scrollback behind the full-height home.
    shell.note("Add dir", "Added /tmp/project.");
    shell.paint(None).unwrap();
    assert!(shell.home.is_none(), "the report is shown, not hidden");
    assert_eq!(shell.home_mark, None);
}

#[test]
fn terminal_signals_follow_the_session_state() {
    use crate::ui::status::Phase;
    use crate::ui::term_state::{Activity, Progress};
    let mut ui = crate::llm::model_config::UiConfig::default();
    let mut status = StatusData {
        model: "qwen3".into(),
        project_path: "/work/dial".into(),
        prompt_tokens: Some(2_500),
        context_window: 10_000,
        ..Default::default()
    };
    let idle = terminal_signals(&ui, &status, Activity::Done, false).unwrap();
    assert_eq!(
        (idle.activity, idle.progress),
        (Activity::Done, Progress::None)
    );
    assert_eq!(
        idle.cwd.as_deref(),
        Some(std::path::Path::new("/work/dial"))
    );
    assert_eq!(
        idle.vars,
        vec![
            ("dialState", "done".to_owned()),
            ("dialModel", "qwen3".to_owned()),
            ("dialContext", "25%".to_owned()),
        ]
    );

    status.phase = Phase::Working;
    let busy = terminal_signals(&ui, &status, Activity::Done, false).unwrap();
    assert_eq!(
        (busy.activity, busy.progress),
        (Activity::Working, Progress::Indeterminate)
    );
    status.model_progress = Some(
        serde_json::from_value(serde_json::json!({
            "phase": "processing",
            "processed_tokens": 400,
            "prompt_tokens": 1000,
        }))
        .unwrap(),
    );
    let prefill = terminal_signals(&ui, &status, Activity::Idle, false).unwrap();
    assert_eq!(prefill.progress, Progress::Percent(40));

    let asking = terminal_signals(&ui, &status, Activity::Idle, true).unwrap();
    assert_eq!(
        (asking.activity, asking.progress),
        (Activity::Waiting, Progress::Paused)
    );

    ui.terminal_integration = false;
    assert!(terminal_signals(&ui, &status, Activity::Idle, true).is_none());
}

/// The state circles `tab_title` can add (orange working, green done, red waiting or failed).
const CIRCLES: [&str; 3] = ["\u{1F7E0}", "\u{1F7E2}", "\u{1F534}"];

#[test]
fn the_tab_title_ends_with_the_state_circle_after_the_session_scope() {
    assert_eq!(tab_title("dial · app", "", ""), "dial · app");
    assert_eq!(
        tab_title("dial · app", "fix the parser", Activity::Working.marker()),
        "dial · app · fix the parser \u{1F7E0}"
    );
    assert_eq!(
        tab_title("dial · app", "", Activity::Done.marker()),
        "dial · app \u{1F7E2}"
    );
    let long = "x".repeat(200);
    let title = tab_title("dial · app", &long, Activity::Done.marker());
    assert!(title.starts_with("dial · app · "), "{title}");
    assert!(title.ends_with("\u{1F7E2}"), "{title}");
    assert!(crate::ui::text::width(&title) < 70, "{title}");
}

#[test]
fn an_idle_or_empty_state_adds_no_circle_to_the_tab_title() {
    for scope in ["", "fix the parser"] {
        let plain = tab_title("dial · app", scope, "");
        assert_eq!(
            tab_title("dial · app", scope, Activity::Idle.marker()),
            plain
        );
        assert!(
            !CIRCLES.iter().any(|circle| plain.contains(circle)),
            "{plain}"
        );
    }
}

#[test]
fn every_active_state_keeps_its_circle_on_selected_and_unselected_tabs() {
    let long = "x".repeat(200);
    for activity in [
        Activity::Working,
        Activity::Waiting,
        Activity::Done,
        Activity::Failed,
    ] {
        for scope in ["", "fix the parser", long.as_str()] {
            let title = tab_title("dial · app", scope, activity.marker());
            assert!(title.contains(activity.marker()), "selected: {title}");
            assert_eq!(title.matches(activity.marker()).count(), 1, "{title}");
            // An unselected tab shows only the end of the title, so the circle must survive there.
            let unselected = crate::ui::text::ellipsize_start(&title, 30, false);
            assert!(
                unselected.contains(activity.marker()),
                "unselected: {unselected} ({activity:?})"
            );
        }
    }
}
