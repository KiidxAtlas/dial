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
        status.main_model_explanation = Some("stale route receipt".into());
        update_model_status(&mut status, &mut home, &config);
        assert_eq!(status.model, model);
        assert!(
            status.inference.is_empty(),
            "A model switch clears old request settings"
        );
        assert!(
            status.main_model_explanation.is_none(),
            "a new selection cannot inherit an old receipt"
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

/// The state circles `tab_title` can add (orange working, green done, blue waiting, red failed).
const CIRCLES: [&str; 4] = ["\u{1F7E0}", "\u{1F7E2}", "\u{1F535}", "\u{1F534}"];

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

#[test]
fn bottom_chrome_leaves_the_transcript_most_of_an_80x24_terminal() {
    // A typical conversation composer: activity row, top border, one input row, hints, controls.
    let composer = 5;
    let budget = aux_budget(24, composer);
    assert!(budget <= 24 * 2 / 5, "{budget}");
    assert!(
        24 - composer - budget >= 10,
        "transcript keeps {}",
        24 - composer - budget
    );
    // Tiny terminals give the chrome nothing rather than starving the transcript.
    for rows in 0..12 {
        assert!(aux_budget(rows, composer) + composer + 3 <= rows.max(composer + 3));
    }
    // A tall terminal still makes room for the plan card and the pinned workers.
    assert!(aux_budget(60, composer) >= 20);

    // The whole paint at 80x24 with a long plan and the rail on stays inside the budget.
    let (_home, _project, _app, mut shell) = super::super::testkit::shell();
    shell.term = crate::ui::term::with_writer(Box::new(std::io::sink()), (80, 24));
    shell.ui.session_rail = true;
    let tasks: Vec<_> = (0..20)
        .map(|i| serde_json::json!({"content": format!("step {i}")}))
        .collect();
    shell
        .todo
        .call(&serde_json::json!({"op": "set", "tasks": tasks}));
    shell.paint(None).unwrap();
}

/// Shared capture of everything the inline terminal writes, replayed into a screen emulator.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The whole main screen at 80x24 with every kind of chrome between the transcript and the
/// composer (pinned workers, an update notice, the queue strip, the status rows): the chrome
/// stays inside its budget, the transcript keeps its rows above it, and every element is drawn
/// whole, in order, without a row wrapping into the next.
#[test]
fn the_full_main_screen_at_80x24_keeps_the_transcript_and_whole_rows() {
    use crate::llm::model_config::SymbolPreset;
    use crate::ui::subagent_model::{AgentEntry, AgentStatus, HubSnapshot};
    for preset in [SymbolPreset::Ascii, SymbolPreset::Unicode] {
        let (_home, _project, _app, mut shell) = super::super::testkit::shell();
        shell.ui.symbols = preset;
        let ui = shell.ui.clone();
        shell.themes.apply_config(&ui, None).unwrap();
        let capture = Capture::default();
        shell.term = crate::ui::term::with_writer(Box::new(capture.clone()), (80, 24));
        shell.home = None;
        let agents = (0..4)
            .map(|i| AgentEntry {
                id: format!("w{i}"),
                name: format!("worker{i}"),
                task: format!("a long task description for worker {i} that runs past the edge"),
                status: AgentStatus::Running,
                ..AgentEntry::default()
            })
            .collect();
        shell.workers = crate::ui::worker_pins::WorkerBoard::from_snapshot(HubSnapshot {
            agents,
            ..Default::default()
        });
        shell.runtime_updates.retain_report("report".into());
        shell.composer.set_busy(true);
        shell.composer.set_draft("queued follow-up message");
        shell.composer.submit_voice();
        shell.status.phase = crate::ui::status::Phase::Working;
        shell.transcript.push_user("hello");
        let reply: String = (0..40).map(|i| format!("reply line {i}\n\n")).collect();
        shell.transcript.assistant_delta(&reply);
        for _ in 0..3 {
            shell.paint(None).unwrap();
        }
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(&capture.0.lock().unwrap());
        let screen = parser.screen();
        let rows: Vec<String> = screen.rows(0, 80).collect();
        let at = |what: &str| {
            let found: Vec<usize> = (0..rows.len())
                .filter(|&i| rows[i].to_lowercase().contains(what))
                .collect();
            assert_eq!(found.len(), 1, "{what:?} once ({preset:?}): {rows:#?}");
            found[0]
        };
        let workers = at("4 active");
        let more = at("+3 more");
        let notice = at("runtime update report ready");
        let queue = at("queued follow-up message");
        let working = at("working");
        let controls = at("ctrl+c stop");
        assert!(
            workers < more && more < notice && notice < queue && queue < working,
            "chrome in order ({preset:?}): {rows:#?}"
        );
        assert_eq!(controls, 23, "the composer ends on the last row: {rows:#?}");
        // Everything above the chrome is transcript, its newest row included.
        assert!(workers >= 10, "transcript keeps {workers} rows: {rows:#?}");
        assert!(
            rows[..workers]
                .iter()
                .all(|r| r.trim().is_empty() || r.contains("reply line")),
            "{rows:#?}"
        );
        assert!(rows[..workers].iter().any(|r| r.contains("reply line 39")));
        assert!(
            (0..24).all(|i| !screen.row_wrapped(i)),
            "a row ran into the next ({preset:?}): {rows:#?}"
        );
        if preset == SymbolPreset::Ascii {
            assert!(rows.iter().all(|r| r.is_ascii()), "{rows:#?}");
        }
    }
}

#[test]
fn shift_tab_never_cycles_into_full_auto() {
    let mut mode = Approval::Ask;
    for _ in 0..6 {
        mode = next_cycled_approval(mode);
        assert_ne!(mode, Approval::Auto);
    }
    assert_eq!(next_cycled_approval(Approval::Ask), Approval::Write);
    assert_eq!(next_cycled_approval(Approval::Write), Approval::Readonly);
    assert_eq!(next_cycled_approval(Approval::Readonly), Approval::Ask);
    assert_eq!(next_cycled_approval(Approval::Auto), Approval::Ask);
}
