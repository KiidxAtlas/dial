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
fn queued_message_picker_reuses_edit_and_remove_commands() {
    let keys = Keymap::defaults();
    for (code, expected) in [
        (KeyCode::Enter, "command:/queue edit 1"),
        (KeyCode::Delete, "command:/queue remove 1"),
    ] {
        let mut panel =
            PaletteOverlay::choice(queue_choices(&[("keep this message".into(), true)]));
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
    let spec = attention_choices(&workers, &jobs);
    assert_eq!(spec.items.len(), 4);
    assert_eq!(
        spec.items
            .iter()
            .filter(|item| item.value == "activity workers")
            .count(),
        3
    );
    let keys = Keymap::defaults();
    let mut picker = PaletteOverlay::choice(spec);
    assert!(
        matches!(picker.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &keys),
        OverlayEvent::Action(action) if action == "command:/activity workers")
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
        let blocker = spec.items.iter().find(|item| item.value == "todo").unwrap();
        assert_eq!(blocker.label, "Todo · 3 · blocked");
        assert!(blocker.detail.contains("operator input required"));
        assert_eq!(
            spec.items.len(),
            if phase == Phase::WaitingCompute { 2 } else { 1 }
        );
        for (command, action) in [
            ("todo", "command:/todo"),
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
        Some(Which::ModelDownloads)
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
