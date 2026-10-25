use super::super::settings::testkit::*;
use super::*;
use crate::ui::subagent_model::{ActivityKind, Totals};
use crossterm::event::KeyEventKind;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new_with_kind(
        code,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    ))
}

/// Status-line preview is only used by the settings panel; the hub never calls it.
fn preview(_: &crate::llm::model_config::UiConfig, _: u16) -> ratatui::text::Line<'static> {
    ratatui::text::Line::default()
}

fn agent(id: &str, status: AgentStatus) -> AgentEntry {
    AgentEntry {
        id: id.into(),
        name: format!("{id}-name"),
        agent: "scout".into(),
        role: "small".into(),
        model: "endpoint/test".into(),
        model_source: "assigned".into(),
        status,
        task: format!("look into {id}"),
        steps: 2,
        elapsed_ms: Some(4_200),
        requests: 3,
        tool_calls: 5,
        cost_usd: Some(0.0123),
        tokens_in: Some(1200),
        tokens_out: Some(300),
        steerable: status == AgentStatus::Running,
        resumable: status.is_finished(),
        cancellable: status == AgentStatus::Running,
        last_activity_ms: Some(epoch_ms()),
        ..Default::default()
    }
}

fn snapshot(agents: Vec<AgentEntry>) -> HubSnapshot {
    HubSnapshot {
        agents,
        ..Default::default()
    }
}

fn event(agent: &str, kind: ActivityKind, title: &str, entry: Option<usize>) -> ActivityEvent {
    ActivityEvent {
        agent_id: agent.into(),
        at_ms: epoch_ms(),
        kind,
        title: title.into(),
        summary: format!("{title} summary"),
        entry,
    }
}

fn keymap() -> Keymap {
    Keymap::defaults()
}

#[test]
fn x_needs_a_second_press_within_the_window() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running)]));
    assert!(matches!(
        hub.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::None
    ));
    assert!(hub.banner_text().unwrap().contains("Press x again"));
    assert!(matches!(
        hub.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::Action(a) if a == "subagent-cancel:task-1"
    ));
}

#[test]
fn x_on_a_finished_worker_explains_why_it_refuses() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Done)]));
    hub.handle(&key(KeyCode::Char('x')), &keys);
    assert_eq!(
        hub.banner_text().unwrap(),
        "task-1-name: it already finished."
    );
}

#[test]
fn message_composes_and_sends_to_the_selected_worker() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Done)]));
    hub.handle(&key(KeyCode::Char('m')), &keys);
    for c in "hi".chars() {
        hub.handle(&key(KeyCode::Char(c)), &keys);
    }
    match hub.handle(&key(KeyCode::Enter), &keys) {
        OverlayEvent::Action(a) => assert_eq!(a, "subagent-message:task-1\nhi"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn paste_fills_an_open_message_draft_and_is_ignored_otherwise() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Done)]));
    hub.handle(&Event::Paste("ignored".into()), &keys);
    assert!(hub.compose.is_none(), "paste never opens the editor");
    hub.handle(&key(KeyCode::Char('m')), &keys);
    hub.handle(&Event::Paste("fix the\nparser \u{1b}[31m".into()), &keys);
    match hub.handle(&key(KeyCode::Enter), &keys) {
        OverlayEvent::Action(a) => assert_eq!(a, "subagent-message:task-1\nfix the\nparser"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn message_refuses_a_running_inline_worker_that_cannot_be_steered() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    let mut inline = agent("inline-1", AgentStatus::Running);
    inline.steerable = false;
    hub.set_snapshot(snapshot(vec![inline]));
    hub.handle(&key(KeyCode::Char('m')), &keys);
    assert!(hub.compose.is_none());
    assert!(hub.banner_text().unwrap().contains("cannot take messages"));
}

#[test]
fn enter_opens_the_worker_and_activity_enter_opens_its_entry() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![agent("task-1", AgentStatus::Done)],
        activity: vec![event("task-1", ActivityKind::Response, "answer", Some(4))],
        ..Default::default()
    });
    assert!(matches!(
        hub.handle(&key(KeyCode::Enter), &keys),
        OverlayEvent::Action(a) if a == "subagent-open:task-1"
    ));
    hub.handle(&key(KeyCode::Char('2')), &keys);
    assert!(matches!(
        hub.handle(&key(KeyCode::Enter), &keys),
        OverlayEvent::Action(a) if a == "subagent-open:task-1#4"
    ));
}

#[test]
fn esc_clears_filter_then_inspector_then_closes() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![
        agent("task-1", AgentStatus::Done),
        agent("task-2", AgentStatus::Done),
    ]));
    hub.handle(&key(KeyCode::Char('/')), &keys);
    for c in "task-2".chars() {
        hub.handle(&key(KeyCode::Char(c)), &keys);
    }
    hub.handle(&key(KeyCode::Enter), &keys);
    assert_eq!(hub.selected.as_deref(), Some("task-2"));
    assert_eq!(hub.rows().len(), 1);
    hub.handle(&key(KeyCode::Esc), &keys);
    assert!(hub.agent_query.is_empty());
    hub.inspector = true;
    assert!(matches!(
        hub.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(!hub.inspector);
    assert!(matches!(
        hub.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn filter_applies_while_typing_and_matches_task_text() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![
        agent("task-1", AgentStatus::Done),
        agent("task-2", AgentStatus::Done),
    ]));
    hub.handle(&key(KeyCode::Char('/')), &keys);
    for c in "task-1".chars() {
        hub.handle(&key(KeyCode::Char(c)), &keys);
    }
    assert_eq!(hub.rows().len(), 1);
    assert_eq!(hub.selected.as_deref(), Some("task-1"));
}

#[test]
fn roster_order_is_stable_across_snapshots() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![
        agent("task-2", AgentStatus::Running),
        agent("task-1", AgentStatus::Done),
    ]));
    hub.set_snapshot(snapshot(vec![
        agent("task-1", AgentStatus::Running),
        agent("task-2", AgentStatus::Done),
    ]));
    let ids: Vec<&str> = hub.agents().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["task-2", "task-1"]);
}

#[test]
fn tree_toggle_keeps_the_selection_and_flat_shows_parents() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    let mut child = agent("task-2", AgentStatus::Running);
    child.parent = Some("task-1".into());
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running), child]));
    hub.handle(&key(KeyCode::Down), &keys);
    assert_eq!(hub.selected.as_deref(), Some("task-2"));
    hub.handle(&key(KeyCode::Char('t')), &keys);
    assert!(hub.tree);
    assert_eq!(hub.selected.as_deref(), Some("task-2"));
    assert_eq!(hub.rows()[1].depth, 1);
}

#[test]
fn activity_filter_scope_and_follow_cycle() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![
            agent("task-1", AgentStatus::Done),
            agent("task-2", AgentStatus::Done),
        ],
        activity: vec![
            event("task-1", ActivityKind::Error, "boom", None),
            event("task-2", ActivityKind::Response, "answer", None),
        ],
        ..Default::default()
    });
    hub.handle(&key(KeyCode::Char('2')), &keys);
    assert_eq!(hub.activity_rows().len(), 2);
    hub.handle(&key(KeyCode::Char('f')), &keys);
    assert_eq!(hub.activity_filter, ActivityFilter::Errors);
    assert_eq!(hub.activity_rows().len(), 1);
    hub.handle(&key(KeyCode::Char('f')), &keys);
    assert_eq!(hub.activity_filter, ActivityFilter::Responses);
    assert_eq!(hub.activity_rows().len(), 1);
    hub.handle(&key(KeyCode::Char('s')), &keys);
    assert_eq!(hub.activity_scope, ActivityScope::Selected("task-1".into()));
    assert!(hub.activity_rows().is_empty(), "task-1 has no responses");
    hub.handle(&key(KeyCode::Char('s')), &keys);
    hub.handle(&key(KeyCode::Char('s')), &keys);
    assert_eq!(hub.activity_scope, ActivityScope::All);
    hub.handle(&key(KeyCode::Char(' ')), &keys);
    assert!(!hub.follow);
    hub.handle(&key(KeyCode::Char(' ')), &keys);
    assert!(hub.follow);
}

#[test]
fn follow_keeps_the_cursor_on_the_newest_event() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![agent("task-1", AgentStatus::Done)],
        activity: vec![
            event("task-1", ActivityKind::Tool, "read", None),
            event("task-1", ActivityKind::Response, "done", None),
        ],
        ..Default::default()
    });
    assert_eq!(hub.activity_pos(hub.activity_rows().len()), 1);
}

#[test]
fn wide_layout_shows_roster_inspector_and_tabs() {
    let rig = Rig::new();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![
            agent("task-1", AgentStatus::Running),
            agent("task-2", AgentStatus::Done),
        ],
        activity: vec![event("task-1", ActivityKind::Tool, "read", Some(0))],
        totals: Totals {
            running: 1,
            total: 2,
            ..Default::default()
        },
    });
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let buffer = render(120, 28, |frame| hub.draw(frame, &ctx));
    let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("1 Workers"), "{text}");
    assert!(text.contains("2 Activity"), "{text}");
    assert!(text.contains("task-1-name"), "{text}");
    assert!(text.contains("Recent activity"), "{text}");
    assert!(text.contains("x cancel"), "{text}");
}

#[test]
fn narrow_layout_shows_roster_until_tab_opens_the_inspector() {
    let rig = Rig::new();
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running)]));
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let narrow = render(60, 24, |frame| hub.draw(frame, &ctx));
    let text: String = narrow.content().iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("task-1-name"), "{text}");
    assert!(!text.contains("Usage"), "{text}");
    hub.handle(&key(KeyCode::Tab), &keys);
    let opened = render(60, 60, |frame| hub.draw(frame, &ctx));
    let text: String = opened.content().iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("Usage"), "{text}");
}

#[test]
fn left_and_right_switch_tabs_and_the_esc_hint_names_what_esc_does() {
    let rig = Rig::new();
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running)]));
    hub.handle(&key(KeyCode::Right), &keys);
    assert!(hub.tab == Tab::Activity);
    hub.handle(&key(KeyCode::Left), &keys);
    assert!(hub.tab == Tab::Agents);

    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let esc = |hub: &SubagentsOverlay| hub.footer(&ctx).last().cloned().unwrap();
    assert_eq!(esc(&hub), "Esc close");
    hub.inspector = true;
    assert_eq!(esc(&hub), "Esc back");
    hub.agent_query = "task".into();
    assert_eq!(esc(&hub), "Esc clear search");
    // Each label matches what the next Esc actually does.
    assert!(matches!(
        hub.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert_eq!(esc(&hub), "Esc back");
    assert!(matches!(
        hub.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert_eq!(esc(&hub), "Esc close");
    assert!(matches!(
        hub.handle(&key(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn activity_tab_renders_its_table() {
    let rig = Rig::new();
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![agent("task-1", AgentStatus::Done)],
        activity: vec![event("task-1", ActivityKind::Error, "boom", Some(0))],
        ..Default::default()
    });
    hub.handle(&key(KeyCode::Char('2')), &keys);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let buffer = render(100, 20, |frame| hub.draw(frame, &ctx));
    let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("all events from all workers"), "{text}");
    assert!(text.contains("boom"), "{text}");
}

#[test]
fn helpers_format_units() {
    assert_eq!(compact(1_500), "1.5k");
    assert_eq!(money(0.0012), "$0.0012");
    assert_eq!(duration(65_000), "1m 05s");
    assert_eq!(ago(0, 125_000), "2m");
    assert_eq!(gauge(50, 100, 60), "██████░░░░░░ 50% 50/100");
}

fn draw_at(hub: &mut SubagentsOverlay, width: u16, height: u16) {
    use crate::{
        llm::model_config::UiConfig,
        ui::{status::StatusData, theme::Theme},
    };
    use ratatui::{Terminal, backend::TestBackend, text::Line};
    let theme = Theme::dark();
    let keys = keymap();
    let status = StatusData::default();
    let preview = |_: &UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &preview,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| hub.draw(frame, &ctx)).unwrap();
}

fn ids_in_roster_order(hub: &SubagentsOverlay) -> Vec<String> {
    hub.rows()
        .iter()
        .map(|row| hub.agents[row.index].id.clone())
        .collect()
}

#[test]
fn click_selects_a_card_and_a_second_click_opens_it() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![
        agent("task-1", AgentStatus::Running),
        agent("task-2", AgentStatus::Running),
    ]));
    draw_at(&mut hub, 120, 40);
    let order = ids_in_roster_order(&hub);
    // Inner origin is (1, 1) inside the border; the body starts under the tab line, and the
    // second card's first line sits three rows down.
    let (col, row) = (3, 1 + 1 + 3);
    let first = std::time::Instant::now();
    assert!(matches!(hub.click(col, row, first), OverlayEvent::None));
    assert_eq!(hub.selected.as_deref(), Some(order[1].as_str()));
    let second = first + std::time::Duration::from_millis(100);
    assert!(matches!(
        hub.click(col, row, second),
        OverlayEvent::Action(a) if a == format!("subagent-open:{}", order[1])
    ));
}

#[test]
fn wheel_scrolls_the_inspector_only_over_its_pane() {
    let mut hub = SubagentsOverlay::new();
    let mut long = agent("task-1", AgentStatus::Running);
    long.task = "word ".repeat(400);
    hub.set_snapshot(snapshot(vec![long]));
    draw_at(&mut hub, 120, 40);
    let inside = 1 + hub.roster_w.get() + 2;
    assert!(!hub.wheel_over_inspector(3, 2, true));
    assert_eq!(hub.inspector_scroll, 0);
    assert!(hub.wheel_over_inspector(inside, 2, true));
    assert_eq!(hub.inspector_scroll, 3);
    assert!(hub.wheel_over_inspector(inside, 2, false));
    assert_eq!(hub.inspector_scroll, 0);
}

#[test]
fn clicking_an_activity_row_selects_it_and_a_second_click_opens_its_entry() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![agent("task-1", AgentStatus::Done)],
        activity: vec![event("task-1", ActivityKind::Response, "answer", Some(4))],
        ..Default::default()
    });
    hub.tab = Tab::Activity;
    draw_at(&mut hub, 120, 40);
    // Activity rows start two lines below the body top: the status line and the header.
    let (col, row) = (3, 1 + 1 + 2);
    let first = std::time::Instant::now();
    assert!(matches!(hub.click(col, row, first), OverlayEvent::None));
    assert_eq!(hub.activity_cursor, 0);
    let second = first + std::time::Duration::from_millis(100);
    assert!(matches!(
        hub.click(col, row, second),
        OverlayEvent::Action(a) if a == "subagent-open:task-1#4"
    ));
}

#[test]
fn clicks_on_the_tab_line_or_before_the_first_draw_do_nothing() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running)]));
    assert!(matches!(
        hub.click(3, 2, std::time::Instant::now()),
        OverlayEvent::None
    ));
    draw_at(&mut hub, 120, 40);
    let before = hub.selected.clone();
    assert!(matches!(
        hub.click(3, 1, std::time::Instant::now()),
        OverlayEvent::None
    ));
    assert_eq!(hub.selected, before);
}

#[test]
fn narrow_layout_maps_clicks_to_the_single_roster_pane() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![
        agent("task-1", AgentStatus::Running),
        agent("task-2", AgentStatus::Running),
    ]));
    draw_at(&mut hub, 60, 20);
    let order = ids_in_roster_order(&hub);
    assert!(!hub.wheel_over_inspector(3, 4, true));
    assert!(matches!(
        hub.click(3, 2 + 3, std::time::Instant::now()),
        OverlayEvent::None
    ));
    assert_eq!(hub.selected.as_deref(), Some(order[1].as_str()));
}

#[test]
fn kv_wrapped_keeps_long_values_on_continuation_rows() {
    let theme = Theme::dark();
    let value = "read-file src/ui/overlay/subagents.rs lines 1 through 400 and summarize";
    let rows = kv_wrapped("tool", value, 36, &theme);
    assert!(rows.len() > 1, "a long value must wrap: {rows:?}");
    let text: String = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                + "\n"
        })
        .collect();
    assert!(text.contains("summarize"), "the tail must survive: {text}");
    assert!(!text.contains('…'), "no clipping marker: {text}");
    assert_eq!(kv_wrapped("tool", "idle", 36, &theme).len(), 1);
}

#[test]
fn activity_deep_link_opens_on_the_activity_tab() {
    let mut overlay = SubagentsOverlay::new();
    assert_eq!(
        overlay.tab,
        Tab::Agents,
        "the hub opens on Agents by default"
    );
    overlay.show_activity();
    assert_eq!(overlay.tab, Tab::Activity);
}

#[test]
fn release_and_held_cancel_do_not_confirm_a_destructive_action() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("task-1", AgentStatus::Running)]));
    hub.handle(&key(KeyCode::Char('x')), &keys);
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        let event = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            kind,
        ));
        assert!(matches!(hub.handle(&event, &keys), OverlayEvent::None));
    }
    assert!(matches!(
        hub.handle(&key(KeyCode::Char('x')), &keys),
        OverlayEvent::Action(action) if action == "subagent-cancel:task-1"
    ));
}

#[test]
fn narrow_roster_page_keys_move_selection_and_backspace_removes_graphemes() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(
        (0..8)
            .map(|i| agent(&i.to_string(), AgentStatus::Running))
            .collect(),
    ));
    hub.body_h.set(9);
    hub.handle(&key(KeyCode::PageDown), &keys);
    assert_eq!(hub.selected.as_deref(), Some("3"));
    hub.handle(&key(KeyCode::PageUp), &keys);
    assert_eq!(hub.selected.as_deref(), Some("0"));
    hub.compose = Some("hello 👩‍💻".into());
    hub.handle(&key(KeyCode::Backspace), &keys);
    assert_eq!(hub.compose.as_deref(), Some("hello "));
    hub.compose = None;
    hub.search = Some("cafe\u{301}".into());
    hub.handle(&key(KeyCode::Backspace), &keys);
    assert_eq!(hub.search.as_deref(), Some("caf"));
}

fn screen(hub: &mut SubagentsOverlay, theme: &Theme, width: u16, height: u16) -> Vec<String> {
    use crate::{llm::model_config::UiConfig, ui::status::StatusData};
    use ratatui::{Terminal, backend::TestBackend, text::Line};
    let keys = keymap();
    let status = StatusData::default();
    let preview = |_: &UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme,
        keys: &keys,
        status: &status,
        preview_status: &preview,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| hub.draw(frame, &ctx)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

/// Five workers like the review sheet: four running, the last one waiting on the operator.
fn review_roster() -> HubSnapshot {
    let agents = (1..=5)
        .map(|n| AgentEntry {
            id: format!("task-{n}"),
            name: if n == 5 {
                "Needs your direction".into()
            } else {
                format!("Worker {n}")
            },
            agent: "worker".into(),
            task: "Preserve public APIs while improving the execution path".into(),
            status: if n == 5 {
                AgentStatus::Waiting
            } else {
                AgentStatus::Running
            },
            current: Some("Inspecting affected callers".into()),
            cancellable: true,
            steerable: true,
            ..Default::default()
        })
        .collect();
    HubSnapshot {
        agents,
        totals: Totals {
            running: 5,
            total: 5,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn workers_needing_input_or_failed_sort_above_running_ones_and_stay_there() {
    let mut hub = SubagentsOverlay::new();
    let mut failed = agent("broke", AgentStatus::Failed);
    failed.cancellable = false;
    hub.set_snapshot(snapshot(vec![
        agent("a", AgentStatus::Running),
        failed.clone(),
        agent("b", AgentStatus::Running),
        agent("ask", AgentStatus::Waiting),
    ]));
    let ids: Vec<&str> = hub.agents().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["ask", "broke", "a", "b"]);
    // A worker that starts waiting later jumps up; the others keep their relative order.
    hub.set_snapshot(snapshot(vec![
        agent("a", AgentStatus::Running),
        failed,
        agent("b", AgentStatus::Waiting),
        agent("ask", AgentStatus::Running),
    ]));
    let ids: Vec<&str> = hub.agents().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["b", "broke", "ask", "a"]);
}

#[test]
fn page_and_home_end_keys_move_the_roster_selection_in_every_layout() {
    let keys = keymap();
    let theme = Theme::dark();
    let workers: Vec<AgentEntry> = (0..20)
        .map(|i| agent(&format!("w{i:02}"), AgentStatus::Running))
        .collect();
    for (width, height) in [(120, 40), (60, 20), (38, 16)] {
        let mut hub = SubagentsOverlay::new();
        hub.set_snapshot(snapshot(workers.clone()));
        screen(&mut hub, &theme, width, height);
        let order = ids_in_roster_order(&hub);
        let page = hub.roster_page();
        hub.handle(&key(KeyCode::PageDown), &keys);
        assert_eq!(
            hub.selected.as_deref(),
            Some(order[page].as_str()),
            "PageDown at {width}x{height}"
        );
        hub.handle(&key(KeyCode::End), &keys);
        assert_eq!(hub.selected.as_deref(), order.last().map(String::as_str));
        hub.handle(&key(KeyCode::PageUp), &keys);
        assert_eq!(
            hub.selected.as_deref(),
            Some(order[order.len() - 1 - page].as_str())
        );
        hub.handle(&key(KeyCode::Home), &keys);
        assert_eq!(hub.selected.as_deref(), Some(order[0].as_str()));
        // The selected card stays on screen after paging.
        hub.handle(&key(KeyCode::PageDown), &keys);
        hub.handle(&key(KeyCode::PageDown), &keys);
        let name = format!("{}-name", hub.selected.clone().unwrap());
        let shown = screen(&mut hub, &theme, width, height).join("\n");
        assert!(shown.contains(&name), "{width}x{height}: {shown}");
    }
}

#[test]
fn narrow_inspector_pages_scroll_it_and_clamp_at_the_end() {
    let keys = keymap();
    let theme = Theme::dark();
    let mut hub = SubagentsOverlay::new();
    let mut busy = agent("busy", AgentStatus::Running);
    busy.task = "word ".repeat(80);
    hub.set_snapshot(snapshot(vec![busy]));
    hub.handle(&key(KeyCode::Tab), &keys);
    screen(&mut hub, &theme, 60, 20);
    let max = hub.inspector_max.get();
    assert!(max > 0, "the long task must overflow");
    hub.handle(&key(KeyCode::PageDown), &keys);
    assert!(hub.inspector_scroll > 0);
    for _ in 0..20 {
        hub.handle(&key(KeyCode::PageDown), &keys);
    }
    assert_eq!(hub.inspector_scroll, max, "scrolling stops at the end");
    hub.handle(&key(KeyCode::Home), &keys);
    assert_eq!(hub.inspector_scroll, 0);
    hub.handle(&key(KeyCode::End), &keys);
    assert_eq!(hub.inspector_scroll, max);
    // One PageUp from the end moves back immediately (no hidden overscroll to unwind).
    hub.handle(&key(KeyCode::PageUp), &keys);
    assert!(hub.inspector_scroll < max);
}

#[test]
fn activity_home_and_end_jump_and_end_resumes_following() {
    let keys = keymap();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(HubSnapshot {
        agents: vec![agent("task-1", AgentStatus::Done)],
        activity: (0..30)
            .map(|i| event("task-1", ActivityKind::Tool, &format!("read {i}"), None))
            .collect(),
        ..Default::default()
    });
    hub.handle(&key(KeyCode::Char('2')), &keys);
    hub.handle(&key(KeyCode::Home), &keys);
    assert_eq!(hub.activity_pos(30), 0);
    assert!(!hub.follow);
    hub.handle(&key(KeyCode::End), &keys);
    assert_eq!(hub.activity_pos(30), 29);
    assert!(hub.follow);
}

#[test]
fn title_counts_once_and_the_tab_row_does_not_repeat_them() {
    let theme = Theme::dark();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(review_roster());
    let shown = screen(&mut hub, &theme, 120, 40);
    assert!(
        shown[0].contains("5 workers · 1 needs input · 4 running"),
        "{}",
        shown[0]
    );
    assert!(!shown[1].contains("running"), "tab row: {}", shown[1]);
    assert_eq!(
        status_counts(&review_roster().agents[..4], " · "),
        "4 workers · running"
    );
    assert_eq!(status_counts(&[], " · "), "none yet");
}

#[test]
fn inspector_omits_unreported_usage_and_empty_details() {
    let theme = Theme::dark();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(review_roster());
    let shown = screen(&mut hub, &theme, 120, 40).join("\n");
    assert!(shown.contains("No usage reported yet."), "{shown}");
    for noise in [
        "unknown",
        "steps 0",
        "requests   0",
        "none yet",
        "isolation",
        "transcript",
    ] {
        assert!(!shown.contains(noise), "{noise:?} in {shown}");
    }
    // Real values still show.
    let mut used = agent("used", AgentStatus::Running);
    used.isolation = Some("worktree feat/x".into());
    hub.set_snapshot(snapshot(vec![used]));
    let shown = screen(&mut hub, &theme, 120, 40).join("\n");
    for value in [
        "elapsed",
        "$0.01",
        "in 1.2k",
        "worktree feat/x",
        "tool calls",
    ] {
        assert!(shown.contains(value), "missing {value}: {shown}");
    }
}

#[test]
fn the_waiting_worker_is_first_and_its_card_says_what_it_needs() {
    let theme = Theme::dark();
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(review_roster());
    assert_eq!(hub.selected.as_deref(), Some("task-5"));
    let shown = screen(&mut hub, &theme, 120, 40).join("\n");
    assert!(shown.contains("needs input:"), "{shown}");
    assert!(shown.contains("Needs your input"), "{shown}");
}

#[test]
fn clipped_roster_text_ends_in_an_ellipsis_and_ascii_has_no_unicode() {
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(review_roster());
    let shown = screen(&mut hub, &Theme::dark(), 120, 40);
    let clipped = shown
        .iter()
        .filter_map(|line| line.split('│').next())
        .filter(|left| left.contains("Preserve public"))
        .all(|left| left.trim_end().ends_with('…'));
    assert!(clipped, "{}", shown.join("\n"));
    let ascii = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    for (w, h) in [(120, 40), (60, 20), (38, 16)] {
        let shown = screen(&mut hub, &ascii, w, h).join("");
        let stray: String = shown
            .chars()
            .filter(|c| !c.is_ascii() && !"─│╭╮╰╯".contains(*c))
            .collect();
        assert!(stray.is_empty(), "{w}x{h} non-ASCII {stray:?}");
    }
}

#[test]
fn footer_offers_only_actions_the_selected_worker_supports() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    let mut hub = SubagentsOverlay::new();
    hub.set_snapshot(snapshot(vec![agent("done", AgentStatus::Done)]));
    let footer = hub.footer(&ctx);
    assert!(!footer.iter().any(|s| s == "x cancel"), "{footer:?}");
    assert!(footer.iter().any(|s| s == "m continue"), "{footer:?}");
    hub.set_snapshot(snapshot(vec![agent("live", AgentStatus::Running)]));
    let footer = hub.footer(&ctx);
    assert!(footer.iter().any(|s| s == "x cancel"), "{footer:?}");
    assert!(footer.iter().any(|s| s == "m message"), "{footer:?}");
}
