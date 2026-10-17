use serde_json::json;
use std::io::Write;

/// `OverlayEvent` has no `PartialEq`; tests compare a stable tag instead.
fn tag(event: OverlayEvent) -> String {
    match event {
        OverlayEvent::None => "none".into(),
        OverlayEvent::Close => "close".into(),
        OverlayEvent::Action(action) => format!("action:{action}"),
        _ => "other".into(),
    }
}

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, text::Line};

use super::*;
use crate::ui::{status::StatusData, theme::Theme};

fn record(value: &Value) -> String {
    format!("{}\n", json!({"type": "message", "message": value}))
}

fn append(path: &Path, text: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

fn agent(status: AgentStatus) -> AgentEntry {
    AgentEntry {
        id: "w1".into(),
        name: "scout".into(),
        agent: "scout".into(),
        task: "find the bug".into(),
        status,
        ..AgentEntry::default()
    }
}

fn viewer(entry: Option<usize>, from_hub: bool) -> SubagentViewer {
    SubagentViewer::new(
        "w1".into(),
        "s1".into(),
        entry,
        from_hub,
        UiConfig::default(),
        Instant::now(),
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_text(viewer: &mut SubagentViewer, keys: &Keymap, text: &str) {
    for c in text.chars() {
        viewer.key(&key(KeyCode::Char(c)), keys, Instant::now());
    }
}

#[test]
fn tail_applies_appended_records_and_waits_for_the_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.jsonl");
    append(&path, &record(&json!({"role": "user", "content": "first"})));
    let mut conversation = Conversation::default();
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 1);
    assert!(!conversation.follow(&path), "nothing new");

    // A record still being written has no newline yet and must not be applied.
    append(
        &path,
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"sec",
    );
    assert!(!conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 1);

    append(&path, "ond\"}}\n");
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 2);
    assert_eq!(
        conversation.entries[1], 1,
        "second user turn starts after the first block"
    );
}

#[test]
fn tail_is_incremental_and_reloads_on_truncation_replacement_and_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.jsonl");
    let message = |text: &str| record(&json!({"role": "user", "content": text}));
    append(&path, &message("alpha"));
    let mut conversation = Conversation::default();
    assert!(conversation.follow(&path));
    let offset = conversation.offset;
    assert_eq!(offset, fs::metadata(&path).unwrap().len());

    // Append: only the new record is applied and the offset advances past it.
    append(&path, &message("beta"));
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 2);
    assert_eq!(conversation.offset, fs::metadata(&path).unwrap().len());
    assert!(!conversation.follow(&path), "nothing new");

    // Truncation to a shorter file replays from the start.
    let before = conversation.revision();
    fs::write(&path, message("x")).unwrap();
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 1);
    assert!(conversation.revision() > before);

    // Replacement by a new inode that is longer than the old offset.
    let replacement = dir.path().join("replacement.jsonl");
    fs::write(
        &replacement,
        format!("{}{}", message("one"), message("two")),
    )
    .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 2);

    // In-place rewrite that keeps the size: the leading bytes change, so it reloads.
    fs::write(&path, format!("{}{}", message("uno"), message("dos"))).unwrap();
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 2);
    assert!(!conversation.follow(&path), "stable after the reload");
}

#[test]
fn tail_holds_a_partial_line_until_its_newline_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.jsonl");
    append(&path, &record(&json!({"role": "user", "content": "one"})));
    let mut conversation = Conversation::default();
    assert!(conversation.follow(&path));
    let complete = conversation.offset;
    append(
        &path,
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"tw",
    );
    assert!(!conversation.follow(&path), "partial line is held");
    assert_eq!(
        conversation.offset, complete,
        "offset stays at the last complete line"
    );
    append(&path, "o\"}}\n");
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 2);
    assert!(conversation.offset > complete);
}

#[test]
fn tail_replays_from_the_start_when_the_file_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.jsonl");
    append(&path, &record(&json!({"role": "user", "content": "a"})));
    append(&path, &record(&json!({"role": "user", "content": "b"})));
    let mut conversation = Conversation::default();
    conversation.follow(&path);
    assert_eq!(conversation.entries.len(), 2);
    let before = conversation.revision();

    fs::write(&path, record(&json!({"role": "user", "content": "c"}))).unwrap();
    assert!(conversation.follow(&path));
    assert_eq!(conversation.entries.len(), 1);
    assert!(conversation.revision() > before);
}

#[test]
fn scroll_follows_the_bottom_until_the_operator_moves_up() {
    let mut scroll = Scroll::default();
    scroll.clamp(100, 10);
    assert_eq!(scroll.top, 90);

    scroll.by(-1, 100, 10);
    assert_eq!((scroll.top, scroll.follow), (89, false));
    scroll.clamp(105, 10);
    assert_eq!(scroll.top, 89, "new rows do not move a paused view");

    scroll.by(20, 105, 10);
    assert_eq!(
        (scroll.top, scroll.follow),
        (95, true),
        "reaching the bottom resumes following"
    );
    scroll.clamp(110, 10);
    assert_eq!(scroll.top, 100);

    scroll.to_top();
    assert_eq!((scroll.top, scroll.follow), (0, false));
    scroll.by(-5, 110, 10);
    assert_eq!(scroll.top, 0, "never above the top");
}

#[test]
fn entry_links_resolve_to_their_first_row_once_replayed() {
    // Entries start at blocks 0, 2 and 5; blocks start at rows 0, 1, 3, 7, 9, 12.
    let entries = [0, 2, 5];
    let anchors = [0, 1, 3, 7, 9, 12];
    assert_eq!(entry_row(&entries, &anchors, 0), Some(0));
    assert_eq!(entry_row(&entries, &anchors, 1), Some(3));
    assert_eq!(entry_row(&entries, &anchors, 2), Some(12));
    assert_eq!(entry_row(&entries, &anchors, 3), None, "not replayed yet");
}

#[test]
fn routing_steers_running_workers_and_resumes_finished_ones() {
    let steerable = AgentEntry {
        steerable: true,
        ..agent(AgentStatus::Running)
    };
    assert_eq!(route(Some(&steerable)), Route::Steer);
    let resumable = AgentEntry {
        resumable: true,
        ..agent(AgentStatus::Done)
    };
    assert_eq!(route(Some(&resumable)), Route::Resume);
    assert!(matches!(
        route(Some(&agent(AgentStatus::Cancelled))),
        Route::Unavailable(_)
    ));
    assert!(matches!(
        route(Some(&agent(AgentStatus::Running))),
        Route::Unavailable(_)
    ));
    assert!(matches!(route(None), Route::Unavailable(_)));
}

#[test]
fn send_emits_steer_or_resume_and_keeps_the_draft_when_unavailable() {
    let keys = Keymap::defaults();
    let mut view = viewer(None, false);
    view.agent = Some(AgentEntry {
        steerable: true,
        ..agent(AgentStatus::Running)
    });
    type_text(&mut view, &keys, "use the cache");
    let event = view.key(&key(KeyCode::Enter), &keys, Instant::now());
    assert_eq!(tag(event), "action:subagent-message:w1\nuse the cache");
    assert!(view.draft.text.is_empty());

    view.agent = Some(AgentEntry {
        resumable: true,
        ..agent(AgentStatus::Done)
    });
    type_text(&mut view, &keys, "more");
    let event = view.key(&key(KeyCode::Enter), &keys, Instant::now());
    assert_eq!(tag(event), "action:subagent-resume:w1\nmore");

    view.agent = Some(agent(AgentStatus::Cancelled));
    type_text(&mut view, &keys, "hello");
    let event = view.key(&key(KeyCode::Enter), &keys, Instant::now());
    assert_eq!(tag(event), "none");
    assert_eq!(view.draft.text, "hello");
    assert!(view.notice.is_some());
}

#[test]
fn escape_clears_the_draft_then_leaves_to_the_hub_or_main() {
    let keys = Keymap::defaults();
    let now = Instant::now();
    let mut from_hub = viewer(None, true);
    type_text(&mut from_hub, &keys, "h");
    assert_eq!(tag(from_hub.key(&key(KeyCode::Esc), &keys, now)), "none");
    assert!(from_hub.draft.text.is_empty());
    assert_eq!(
        tag(from_hub.key(&key(KeyCode::Esc), &keys, now)),
        "action:subagent-hub"
    );

    let mut main = viewer(None, false);
    assert_eq!(tag(main.key(&key(KeyCode::Esc), &keys, now)), "close");
}

#[test]
fn double_left_on_an_empty_draft_returns_to_the_hub() {
    let keys = Keymap::defaults();
    let start = Instant::now();
    let mut view = viewer(None, false);
    assert_eq!(tag(view.key(&key(KeyCode::Left), &keys, start)), "none");
    assert_eq!(
        tag(view.key(
            &key(KeyCode::Left),
            &keys,
            start + Duration::from_millis(100)
        )),
        "action:subagent-hub"
    );
    view.key(&key(KeyCode::Left), &keys, start + Duration::from_secs(5));
    assert_eq!(
        tag(view.key(&key(KeyCode::Left), &keys, start + Duration::from_secs(9))),
        "none",
        "slow presses do not count as a double"
    );
}

#[test]
fn cancel_needs_a_second_x_and_only_when_cancellable() {
    let keys = Keymap::defaults();
    let now = Instant::now();
    let mut view = viewer(None, false);
    view.agent = Some(agent(AgentStatus::Done));
    view.key(&key(KeyCode::Char('x')), &keys, now);
    assert!(view.notice.is_some());
    assert!(!view.armed_cancel, "finished workers are not cancellable");

    view.agent = Some(AgentEntry {
        cancellable: true,
        ..agent(AgentStatus::Running)
    });
    assert_eq!(tag(view.key(&key(KeyCode::Char('x')), &keys, now)), "none");
    assert_eq!(
        tag(view.key(&key(KeyCode::Char('x')), &keys, now)),
        "action:subagent-cancel:w1"
    );
    view.key(&key(KeyCode::Char('x')), &keys, now);
    view.key(&key(KeyCode::Char('j')), &keys, now);
    assert!(!view.armed_cancel, "any other key disarms");
}

#[test]
fn ctrl_o_toggles_tool_expansion_and_keys_scroll_without_a_draft() {
    let keys = Keymap::defaults();
    let now = Instant::now();
    let mut view = viewer(None, false);
    view.geometry = (100, 10);
    view.key(&ctrl('o'), &keys, now);
    assert!(view.expanded);
    view.key(&key(KeyCode::Char('g')), &keys, now);
    assert_eq!(view.scroll.top, 0);
    view.key(&key(KeyCode::Char('G')), &keys, now);
    assert!(view.scroll.follow);
    view.key(&key(KeyCode::Char('k')), &keys, now);
    assert_eq!((view.scroll.top, view.scroll.follow), (89, false));
    view.key(&key(KeyCode::Char('k')), &keys, now);
    view.key(&key(KeyCode::Char('j')), &keys, now);
    assert_eq!(view.scroll.top, 89);
    assert!(
        view.draft.text.is_empty(),
        "j/k/g/G/x do not type when the draft is empty"
    );
}

#[test]
fn draw_shows_tool_output_only_when_tool_cards_are_expanded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.jsonl");
    append(
        &path,
        &record(&json!({"role": "user", "content": "list the files"})),
    );
    append(
        &path,
        &record(&json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "c1", "type": "function",
                "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}]
        })),
    );
    let output = (0..30).map(|i| format!("line_{i}\n")).collect::<String>() + "OUTPUT_MARKER_XYZ";
    append(
        &path,
        &record(&json!({"role": "tool", "tool_call_id": "c1", "content": output})),
    );
    append(
        &path,
        &record(&json!({"role": "assistant", "content": "All listed."})),
    );

    let theme = Theme::dark();
    let keys = Keymap::defaults();
    let status = StatusData::default();
    let preview = |_: &UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme: &theme,
        keys: &keys,
        status: &status,
        preview_status: &preview,
    };
    let render = |view: &mut SubagentViewer, width: u16, height: u16| -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view.draw(frame, &ctx)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer.content().iter().map(|cell| cell.symbol()).collect()
    };

    let mut view = viewer(None, false);
    view.agent = Some(AgentEntry {
        transcript: Some(path.clone()),
        ..agent(AgentStatus::Done)
    });
    view.ui.tool_output_lines = 3;
    view.conversation.follow(&path);
    view.conversation.finish();
    let collapsed = render(&mut view, 80, 24);
    assert!(collapsed.contains("All listed."), "assistant prose renders");
    assert!(collapsed.contains("scout"), "header names the worker");
    assert!(
        !collapsed.contains("OUTPUT_MARKER_XYZ"),
        "tool output is folded"
    );

    view.expanded = true;
    let expanded = render(&mut view, 80, 24);
    assert!(
        expanded.contains("OUTPUT_MARKER_XYZ"),
        "expanded cards show output"
    );
    assert_ne!(collapsed, expanded);

    // Narrow and tiny terminals must render without panicking.
    render(&mut view, 30, 8);
    render(&mut view, 4, 2);
    render(
        &mut SubagentViewer::new(
            "w2".into(),
            "s1".into(),
            Some(7),
            true,
            UiConfig::default(),
            Instant::now(),
        ),
        40,
        10,
    );
}
