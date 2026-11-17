use super::*;
use crate::app::command_registry::{canonical_command, command_panel};
use crossterm::event::{KeyEvent, KeyModifiers};

#[test]
fn activity_routes_and_legacy_deep_links_share_destinations() {
    for (name, args, expected) in [
        ("activity", "", ActivityTab::Workers),
        ("activity", "workers", ActivityTab::Workers),
        ("activity", "attention", ActivityTab::Attention),
        ("activity", "processes", ActivityTab::Processes),
        ("activity", "compute", ActivityTab::Compute),
        ("activity", "schedule", ActivityTab::Schedule),
        ("activity", "queue", ActivityTab::Queue),
        ("workers", "", ActivityTab::Workers),
        ("jobs", "", ActivityTab::Workers),
        ("hub", "", ActivityTab::Workers),
        ("processes", "", ActivityTab::Processes),
        ("bg", "", ActivityTab::Processes),
        ("compute", "", ActivityTab::Compute),
        ("schedule", "", ActivityTab::Schedule),
        ("queue", "", ActivityTab::Queue),
    ] {
        let destination = command_panel(canonical_command(name), args).expect("panel route");
        let actual = match destination {
            Which::Subagents => ActivityTab::Workers,
            Which::Jobs => ActivityTab::Processes,
            Which::Compute => ActivityTab::Compute,
            Which::Activity(tab) => tab,
            _ => panic!("wrong destination for /{name} {args}"),
        };
        assert_eq!(actual, expected, "/{name} {args}");
    }
    // Argument-bearing legacy commands retain their existing handlers.
    for (name, args) in [
        ("workers", "worker-id"),
        ("queue", "edit 1"),
        ("schedule", "run"),
        ("activity", "invalid"),
    ] {
        assert!(command_panel(name, args).is_none());
    }
}

#[test]
fn activity_tabs_cycle_in_both_directions_and_preserve_inner_tabs() {
    let keys = Keymap::defaults();
    for (index, tab) in ActivityTab::ALL.into_iter().enumerate() {
        let mut panel = ActivityOverlay::new(
            tab,
            ActiveOverlay::Palette(PaletteOverlay::choice(queue_choices(&[]))),
        );
        for (code, expected) in [
            (
                KeyCode::Tab,
                ActivityTab::ALL[(index + 1) % ActivityTab::ALL.len()],
            ),
            (
                KeyCode::BackTab,
                ActivityTab::ALL[(index + ActivityTab::ALL.len() - 1) % ActivityTab::ALL.len()],
            ),
        ] {
            let event = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
            assert!(
                matches!(panel.handle(&event, &keys), OverlayEvent::Action(action) if action == format!("open:activity:{}", expected.name()) && crate::app::overlay::overlay_destination(&action).is_some())
            );
        }
        let event = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert!(matches!(panel.handle(&event, &keys), OverlayEvent::None));
    }
}

#[test]
fn arrow_keys_cycle_activity_tabs_without_leaving_for_the_models_panel() {
    let keys = Keymap::defaults();
    let mut tab = ActivityTab::Workers;
    // Every Right press stays on an Activity tab and returns to Workers after a full lap.
    for _ in 0..ActivityTab::ALL.len() {
        let mut panel = ActivityOverlay::new(
            tab,
            ActiveOverlay::Palette(PaletteOverlay::choice(queue_choices(&[]))),
        );
        let event = Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let OverlayEvent::Action(action) = panel.handle(&event, &keys) else {
            panic!("Right must switch the Activity tab");
        };
        let next = action
            .strip_prefix("open:activity:")
            .and_then(ActivityTab::parse)
            .expect("an activity tab");
        if next == ActivityTab::Downloads {
            assert!(matches!(
                crate::app::overlay::overlay_destination(&action),
                Some(Which::Activity(ActivityTab::Downloads))
            ));
        }
        tab = next;
    }
    assert_eq!(tab, ActivityTab::Workers);
}

#[test]
fn title_row_is_one_surface_row_with_the_active_tab_styled_at_every_width() {
    let theme = crate::ui::theme::Theme::dark();
    let keys = Keymap::defaults();
    for width in [12, 30, 60, 80, 120, 200] {
        let line = title_row(&theme, &keys, ActivityTab::Workers, width);
        let plain: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(crate::ui::text::width(&plain), width, "{width}: {plain:?}");
        assert!(!plain.contains('['), "{width}: no bracket tabs: {plain:?}");
        let active = line
            .spans
            .iter()
            .find(|span| span.content.contains("Workers"))
            .unwrap_or_else(|| panic!("{width}: active tab named: {plain:?}"));
        assert_eq!(
            active.style.fg,
            crate::ui::chrome::tab_style(&theme, true).fg,
            "{width}"
        );
        if width >= 120 {
            assert!(
                plain.contains("Processes") && plain.contains("tabs"),
                "{plain:?}"
            );
            assert!(plain.contains("Downloads"), "{plain:?}");
            assert!(plain.contains("inner"), "{plain:?}");
            if width >= 200 {
                assert!(plain.contains("Ctrl+Shift+F11/F12"), "{plain:?}");
            }
        }
    }
}

#[test]
fn activity_over_the_worker_hub_draws_one_styled_title_row() {
    let theme = crate::ui::theme::Theme::dark();
    let keys = Keymap::defaults();
    let status = crate::ui::status::StatusData::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &|_, _| Line::default(),
    };
    let mut panel = ActivityOverlay::new(
        ActivityTab::Workers,
        ActiveOverlay::Subagents(Box::new(crate::ui::subagent_hub::SubagentHub::new(
            "session".into(),
        ))),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
    terminal.draw(|frame| panel.draw(frame, &ctx)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect();
    if std::env::var_os("DIAL_PRINT_ACTIVITY").is_some() {
        eprintln!("{}", rows.join("\n"));
    }
    assert!(
        rows[0].starts_with(" Activity   Attention  Workers"),
        "{rows:#?}"
    );
    assert!(rows[0].trim_end().ends_with("tabs"), "{rows:#?}");
    assert!(
        !rows.iter().any(|row| row.contains("[workers]")),
        "{rows:#?}"
    );
    // The active tab carries the shared tab style; inactive tabs stay muted.
    let active_x = rows[0].find("Workers").unwrap() as u16;
    assert!(
        buffer[(active_x, 0)]
            .modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
}

#[test]
fn queued_message_picker_reuses_edit_and_remove_commands() {
    let keys = Keymap::defaults();
    for (code, expected) in [
        (KeyCode::Enter, "command:/queue edit @42"),
        (KeyCode::Delete, "command:/queue remove @42"),
    ] {
        let mut panel =
            PaletteOverlay::choice(queue_choices(&[(42, "keep this message".into(), true)]));
        let event = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        if code == KeyCode::Delete {
            assert!(matches!(panel.handle(&event, &keys), OverlayEvent::None));
        }
        assert!(
            matches!(panel.handle(&event, &keys), OverlayEvent::Action(action) if action == expected)
        );
    }
}

#[test]
fn attention_aggregates_available_inputs_without_replaying_work() {
    use crate::ui::overlay::jobs::JobRow;
    use crate::ui::subagent_model::{AgentEntry, AgentStatus, HubSnapshot};
    let workers = HubSnapshot {
        agents: [
            AgentStatus::Running,
            AgentStatus::Waiting,
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Interrupted,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, status)| AgentEntry {
            id: i.to_string(),
            name: format!("worker-{i}"),
            status,
            ..Default::default()
        })
        .collect(),
        ..Default::default()
    };
    let jobs: Vec<JobRow> = [
        ("exited (exit code 0)", Some(0)),
        ("exited (exit code 1)", Some(1)),
        ("unknown", None),
    ]
    .into_iter()
    .map(|(status, code)| JobRow {
        name: status.into(),
        status: status.into(),
        process_outcome: code.map(|code| crate::runtime::execution::ProcessOutcome {
            started: true,
            exit_code: Some(code),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        }),
        command: "check".into(),
        running: false,
        stop_requested: false,
        terminal: false,
        mode: "session".into(),
    })
    .collect();
    let mut spec = attention_choices(&workers, &jobs);
    assert_eq!(spec.items.len(), 4);
    assert_eq!(
        spec.items
            .iter()
            .filter(|item| item.value.starts_with("subagent-open:"))
            .count(),
        3
    );
    let keys = Keymap::defaults();
    let process = spec.items.remove(3);
    let mut process_picker = ActivityOverlay::new(
        ActivityTab::Attention,
        ActiveOverlay::Palette(PaletteOverlay::choice(ChoiceSpec {
            items: vec![process],
            ..Default::default()
        })),
    );
    let OverlayEvent::Action(action) = process_picker.handle(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &keys,
    ) else {
        panic!("process navigation");
    };
    assert!(
        matches!(crate::app::overlay::overlay_destination(&action), Some(Which::Job(name)) if name == jobs[1].name)
    );
    let mut picker = ActivityOverlay::new(
        ActivityTab::Attention,
        ActiveOverlay::Palette(PaletteOverlay::choice(spec)),
    );
    assert!(
        matches!(picker.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
        OverlayEvent::Action(action) if action == "subagent-open:1")
    );
    assert!(
        attention_choices(&HubSnapshot::default(), &[])
            .items
            .is_empty()
    );
}

#[test]
fn attention_projects_only_explicit_todo_blockers_and_observed_compute_waits() {
    use crate::agent::workflow::todo::TodoList;
    use crate::ui::status::{Phase, StatusData};
    let mut todo = TodoList::default();
    todo.restore(&serde_json::json!([
        {"content": "collect evidence", "status": "pending"},
        {"content": "implement", "status": "in_progress"},
        {"content": "get credentials", "status": "blocked", "note": "operator input required"},
        {"content": "old work", "status": "completed"},
        {"content": "dropped work", "status": "abandoned", "note": "not needed"}
    ]))
    .unwrap();
    let before = todo.snapshot();
    for phase in [
        Phase::Idle,
        Phase::Working,
        Phase::Compacting,
        Phase::Setup,
        Phase::WaitingCompute,
    ] {
        let status = StatusData {
            phase,
            ..Default::default()
        };
        let spec = attention_choices_with_context(
            &Default::default(),
            &[],
            Some(&status),
            Some(&todo),
            None,
            None,
        );
        let blocker = spec
            .items
            .iter()
            .find(|item| item.value == "todo show 3")
            .unwrap();
        assert_eq!(blocker.label, "Todo · 3 · blocked");
        assert!(blocker.detail.contains("operator input required"));
        assert_eq!(
            spec.items.len(),
            if phase == Phase::WaitingCompute { 2 } else { 1 }
        );
        for (command, action) in [
            ("todo show 3", "command:/todo show 3"),
            ("activity compute", "command:/activity compute"),
        ] {
            let mut selected = attention_choices_with_context(
                &Default::default(),
                &[],
                Some(&status),
                Some(&todo),
                None,
                None,
            );
            selected.items.retain(|item| item.value == command);
            if selected.items.is_empty() {
                continue;
            }
            let mut panel = PaletteOverlay::choice(selected);
            assert!(
                matches!(panel.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &Keymap::defaults()), OverlayEvent::Action(actual) if actual == action)
            );
        }
    }
    assert_eq!(todo.snapshot(), before);
    assert!(
        attention_choices_with_context(&Default::default(), &[], None, None, None, None)
            .items
            .is_empty()
    );
}

#[test]
fn observed_download_failure_opens_downloads_without_retrying() {
    let failure = crate::ui::overlay::models::DownloadFailure {
        repo: "org/model".into(),
        detail: "fixture download failed".into(),
    };
    let spec =
        attention_choices_with_context(&Default::default(), &[], None, None, Some(&failure), None);
    assert_eq!(spec.items.len(), 1);
    assert_eq!(spec.items[0].detail, failure.detail);
    let mut panel = PaletteOverlay::choice(spec);
    assert!(
        matches!(panel.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &Keymap::defaults()), OverlayEvent::Action(action) if action == "command:/activity downloads")
    );
    assert!(matches!(
        command_panel("activity", "downloads"),
        Some(Which::Activity(ActivityTab::Downloads))
    ));
}

#[test]
fn historical_setup_failure_opens_settings_for_inspection_only() {
    let failure = crate::app::overlay::auth::SetupFailure {
        label: "Voice".into(),
        detail: "setup failed".into(),
    };
    let spec =
        attention_choices_with_context(&Default::default(), &[], None, None, None, Some(&failure));
    assert_eq!(spec.items.len(), 1);
    assert_eq!(spec.items[0].label, "Setup · Voice · last failure");
    assert_eq!(spec.items[0].detail, failure.detail);
    let mut panel = PaletteOverlay::choice(spec);
    assert!(
        matches!(panel.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &Keymap::defaults()), OverlayEvent::Action(action) if action == "command:/settings")
    );
    assert!(matches!(
        command_panel("settings", ""),
        Some(Which::Settings)
    ));
}

#[test]
fn portable_inner_tab_navigation_stays_inside_the_selected_panel() {
    let theme = crate::ui::theme::Theme::dark();
    let keys = Keymap::defaults();
    let status = crate::ui::status::StatusData::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &|_, _| Line::default(),
    };
    let mut panel = ActivityOverlay::new(
        ActivityTab::Workers,
        ActiveOverlay::Subagents(Box::new(crate::ui::subagent_hub::SubagentHub::new(
            "session".into(),
        ))),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
    for (code, modifiers, expected) in [
        (KeyCode::PageDown, KeyModifiers::ALT, "No activity yet"),
        (KeyCode::PageUp, KeyModifiers::ALT, "No workers yet"),
        (
            KeyCode::F(12),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            "No activity yet",
        ),
        (
            KeyCode::F(11),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            "No workers yet",
        ),
    ] {
        assert!(matches!(
            panel.handle(&Event::Key(KeyEvent::new(code, modifiers)), &keys),
            OverlayEvent::None
        ));
        assert_eq!(panel.tab, ActivityTab::Workers);
        terminal.draw(|frame| panel.draw(frame, &ctx)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains(expected), "{text}");
    }
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
        &keys,
    );
    terminal.draw(|frame| panel.draw(frame, &ctx)).unwrap();
    let sheet: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        sheet.contains("Ctrl+Shift+F11") && sheet.contains("Ctrl+Shift+F12"),
        "{sheet}"
    );
}

#[test]
fn attention_empty_refresh_names_observed_and_unavailable_sources() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let theme = crate::ui::theme::Theme::dark();
    let keys = Keymap::defaults();
    let status = crate::ui::status::StatusData::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &|_, _| Line::default(),
    };
    for jobs in [None, Some(&[][..])] {
        let mut panel = ActivityOverlay::new(
            ActivityTab::Attention,
            ActiveOverlay::Palette(PaletteOverlay::choice(attention_choices(
                &Default::default(),
                &[],
            ))),
        );
        panel.refresh_attention(
            &layout,
            &crate::runtime::Tree::default(),
            None,
            jobs,
            AttentionContext::default(),
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(180, 40)).unwrap();
        terminal.draw(|frame| panel.draw(frame, &ctx)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("No attention items in observed sources"),
            "{text}"
        );
        assert!(
            text.contains(if jobs.is_some() {
                "Processes: observed (0 processes)"
            } else {
                "Processes: unavailable"
            }),
            "{text}"
        );
        assert!(text.contains("Downloads: unavailable"), "{text}");
    }
}

#[test]
fn attached_process_keeps_inner_tab_function_keys() {
    use crate::ui::overlay::jobs::{JobRow, JobsOverlay};
    let keys = Keymap::defaults();
    let attached = || {
        let mut jobs = JobsOverlay::new();
        jobs.set_jobs(vec![JobRow {
            name: "terminal".into(),
            command: "fixture".into(),
            status: "running".into(),
            process_outcome: None,
            running: true,
            stop_requested: false,
            terminal: true,
            mode: "session".into(),
        }]);
        jobs.handle(
            &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &keys,
        );
        assert_eq!(jobs.attached(), Some("terminal"));
        jobs
    };
    let mut direct = attached();
    let mut activity = ActivityOverlay::new(
        ActivityTab::Processes,
        ActiveOverlay::Jobs(Box::new(attached())),
    );
    for code in [KeyCode::F(11), KeyCode::F(12)] {
        let key = KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert!(
            keys.actions_for(&key).is_empty(),
            "fallback must not shadow a default global binding"
        );
        let event = Event::Key(key);
        match (direct.handle(&event, &keys), activity.handle(&event, &keys)) {
            (OverlayEvent::None, OverlayEvent::None) => {}
            (
                OverlayEvent::JobInput {
                    name: expected_name,
                    bytes: expected_bytes,
                },
                OverlayEvent::JobInput { name, bytes },
            ) => {
                assert_eq!(name, expected_name);
                assert_eq!(bytes, expected_bytes);
            }
            _ => panic!("Activity must leave attached terminal input unchanged"),
        }
        assert_eq!(activity.tab, ActivityTab::Processes);
        assert!(
            matches!(&*activity.panel, ActiveOverlay::Jobs(jobs) if jobs.attached() == Some("terminal"))
        );
    }
}

#[test]
fn activity_retains_drilldowns_but_never_sibling_tabs() {
    for tab in ActivityTab::ALL {
        let panel = ActiveOverlay::Activity(Box::new(ActivityOverlay::new(
            tab,
            ActiveOverlay::Palette(PaletteOverlay::choice(queue_choices(&[]))),
        )));
        for action in ["open:process:\"job\"", "subagent-open:worker"] {
            assert_eq!(
                panel.retains_navigation(action),
                tab == ActivityTab::Attention,
                "{tab:?}: {action}"
            );
        }
        for sibling in ActivityTab::ALL {
            assert!(
                !panel.retains_navigation(&format!("open:activity:{}", sibling.name())),
                "{tab:?} -> {sibling:?}"
            );
        }
    }
}
