use super::*;
use crossterm::event::{KeyEvent, KeyModifiers};

fn node(id: &str, depth: usize, text: &str) -> TreeNode {
    TreeNode {
        id: id.into(),
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
    abandoned.on_active_branch = false;
    let mut kept = node("c", 1, "kept idea");
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
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn opens_on_the_current_node_and_shows_branches_and_labels() {
    let picker = Picker::new(sample());
    let rows = picker.rows(80, 20, &Theme::dark());
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
fn no_match_and_empty_states_are_explained_and_enter_selects_nothing() {
    let mut picker = Picker::new(sample());
    for c in "zzz".chars() {
        picker.handle(&key(KeyCode::Char(c)), 20);
    }
    assert!(text_of(&picker.rows(80, 20, &Theme::dark())).contains("No matches"));
    assert!(matches!(
        picker.handle(&key(KeyCode::Enter), 20),
        Action::None
    ));
    let empty = Picker::new(Vec::new());
    assert!(text_of(&empty.rows(80, 20, &Theme::dark())).contains("No messages"));
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
    for cols in [0, 1, 11, 12, 30, 80, 140] {
        for height in 0..=30 {
            let rows = picker.rows(cols, height, &Theme::dark());
            assert!(rows.len() <= height, "{cols}x{height}: {} rows", rows.len());
            assert!(rows.iter().all(|row| text::line_width(row) <= cols));
            if cols >= 60 && height >= 5 {
                assert!(text_of(&rows).contains("number 39"), "{cols}x{height}");
            }
        }
    }
}
