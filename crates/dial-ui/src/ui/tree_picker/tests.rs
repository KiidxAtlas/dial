use super::*;
use crossterm::event::{KeyEvent, KeyModifiers};

fn node(id: &str, depth: usize, text: &str) -> TreeNode {
    TreeNode {
        id: id.into(),
        parent: None,
        depth,
        kind: TreeNodeKind::User,
        text: text.into(),
        label: None,
        ts: String::new(),
        on_active_branch: true,
        current: false,
        children: 0,
    }
}

fn sample() -> Vec<TreeNode> {
    let mut root = node("a", 0, "first question");
    root.children = 2;
    let mut abandoned = node("b", 1, "abandoned idea");
    abandoned.parent = Some("a".into());
    abandoned.on_active_branch = false;
    let mut kept = node("c", 1, "kept idea");
    kept.parent = Some("a".into());
    kept.label = Some("milestone".into());
    kept.current = true;
    vec![root, abandoned, kept]
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn text_of(rows: &Rows) -> String {
    rows.iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn preview(lines: &[&str], messages: usize) -> Preview {
    let lines = lines
        .iter()
        .map(|text| Line::from((*text).to_owned()))
        .collect();
    Preview::new(lines, messages)
}

fn ctrl(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

#[test]
fn opens_on_the_current_node_and_shows_branches_and_labels() {
    let picker = Picker::new(sample());
    let rows = picker.rows(80, 20, &Theme::dark(), &Preview::empty());
    let shown = text_of(&rows);
    for needle in [
        "Session tree",
        "first question",
        "abandoned idea",
        "[milestone]",
        "<- here",
        "(2 branches)",
    ] {
        assert!(shown.contains(needle), "missing {needle}:\n{shown}");
    }
    let mut picker = picker;
    assert!(matches!(
        picker.handle(&key(KeyCode::Enter), 20),
        Action::Select(2)
    ));
}

#[test]
fn navigation_search_and_cancel_keys() {
    let mut picker = Picker::new(sample());
    picker.handle(&key(KeyCode::Home), 20);
    picker.handle(&key(KeyCode::Down), 20);
    assert!(matches!(
        picker.handle(&key(KeyCode::Enter), 20),
        Action::Select(1)
    ));
    for c in "mile".chars() {
        picker.handle(&key(KeyCode::Char(c)), 20);
    }
    assert_eq!(picker.query(), "mile");
    assert!(matches!(
        picker.handle(&key(KeyCode::Enter), 20),
        Action::Select(2)
    ));
    assert!(matches!(
        picker.handle(&key(KeyCode::Esc), 20),
        Action::None
    ));
    assert!(picker.query().is_empty());
    assert!(matches!(
        picker.handle(&key(KeyCode::Esc), 20),
        Action::Cancel
    ));
}

#[test]
fn left_and_right_move_between_sibling_branches_only() {
    let mut picker = Picker::new(sample());
    assert_eq!(picker.selected_id(), Some("c"));
    picker.handle(&key(KeyCode::Left), 20);
    assert_eq!(picker.selected_id(), Some("b"));
    // No sibling further left; the selection stays put.
    picker.handle(&key(KeyCode::Left), 20);
    assert_eq!(picker.selected_id(), Some("b"));
    picker.handle(&key(KeyCode::Right), 20);
    assert_eq!(picker.selected_id(), Some("c"));
    picker.handle(&key(KeyCode::Right), 20);
    assert_eq!(picker.selected_id(), Some("c"));
    // The root has no siblings, so the keys do nothing there.
    picker.handle(&key(KeyCode::Home), 20);
    picker.handle(&key(KeyCode::Right), 20);
    assert_eq!(picker.selected_id(), Some("a"));
}

#[test]
fn context_pane_shows_the_conversation_and_the_branch_position() {
    let picker = Picker::new(sample());
    let preview = preview(&["You", "first question", "Dial", "an answer"], 2);
    let shown = text_of(&picker.rows(80, 20, &Theme::dark(), &preview));
    for needle in [
        "Conversation to here · 2 messages",
        "branch 2 of 2",
        "first question",
        "an answer",
    ] {
        assert!(shown.contains(needle), "missing {needle}:\n{shown}");
    }
}

#[test]
fn long_context_keeps_the_newest_lines_when_the_pane_is_short() {
    let picker = Picker::new(sample());
    let replies: Vec<String> = (0..30).map(|n| format!("reply {n}")).collect();
    let refs: Vec<&str> = replies.iter().map(String::as_str).collect();
    let shown = text_of(&picker.rows(80, 12, &Theme::dark(), &preview(&refs, refs.len())));
    assert!(shown.contains("earlier lines"), "{shown}");
    assert!(shown.contains("reply 29"), "{shown}");
    assert!(!shown.contains("reply 0\n"), "{shown}");
}

#[test]
fn no_match_and_empty_states_are_explained_and_enter_selects_nothing() {
    let mut picker = Picker::new(sample());
    for c in "zzz".chars() {
        picker.handle(&key(KeyCode::Char(c)), 20);
    }
    let shown = text_of(&picker.rows(80, 20, &Theme::dark(), &Preview::empty()));
    assert!(shown.contains("No matches"));
    assert!(matches!(
        picker.handle(&key(KeyCode::Enter), 20),
        Action::None
    ));
    assert_eq!(picker.selected_id(), None);
    let empty = Picker::new(Vec::new());
    let shown = text_of(&empty.rows(80, 20, &Theme::dark(), &Preview::empty()));
    assert!(shown.contains("No messages"));
}

#[test]
fn rows_fit_every_size_and_keep_the_selection_visible() {
    let nodes: Vec<TreeNode> = (0..40)
        .map(|n| {
            let mut node = node(
                &format!("n{n}"),
                n % 4,
                &format!("message number {n} 日本語"),
            );
            node.current = n == 39;
            node
        })
        .collect();
    let picker = Picker::new(nodes);
    let path = preview(&["a question that is fairly long 日本語 text"], 1);
    let empty = Preview::empty();
    for cols in [0, 1, 11, 12, 30, 80, 140] {
        for height in 0..=30 {
            for preview in [&empty, &path] {
                let rows = picker.rows(cols, height, &Theme::dark(), preview);
                assert!(rows.len() <= height, "{cols}x{height}: {} rows", rows.len());
                assert!(rows.iter().all(|row| text::line_width(row) <= cols));
                if cols >= 60 && height >= 5 {
                    assert!(text_of(&rows).contains("number 39"), "{cols}x{height}");
                }
            }
        }
    }
}

#[test]
fn ctrl_arrows_jump_between_turns_and_to_the_parent() {
    let mut picker = Picker::new(sample());
    assert_eq!(picker.selected_id(), Some("c"));
    picker.handle(&ctrl(KeyCode::Left), 20);
    assert_eq!(picker.selected_id(), Some("a"));
    picker.handle(&ctrl(KeyCode::Down), 20);
    assert_eq!(picker.selected_id(), Some("b"));
    picker.handle(&ctrl(KeyCode::Down), 20);
    assert_eq!(picker.selected_id(), Some("c"));
    picker.handle(&ctrl(KeyCode::Up), 20);
    assert_eq!(picker.selected_id(), Some("b"));
}

#[test]
fn select_parent_terminates_on_a_cyclic_parent_chain() {
    let mut matched = node("m", 0, "match");
    matched.parent = Some("a".into());
    let mut a = node("a", 0, "hidden");
    a.parent = Some("b".into());
    let mut b = node("b", 0, "hidden");
    b.parent = Some("a".into());
    let mut picker = Picker::new(vec![a, b, matched]);
    picker.query = "match".into();
    picker.refilter();
    assert_eq!(picker.selected_id(), Some("m"));
    picker.handle(&ctrl(KeyCode::Left), 20);
    assert_eq!(
        picker.selected_id(),
        Some("m"),
        "no ancestor is visible, so the selection stays put"
    );
}
