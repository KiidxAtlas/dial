use super::*;
use serde_json::json;

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn list(tasks: serde_json::Value) -> TodoList {
    let mut todo = TodoList::new();
    assert!(!todo.call(&json!({"op": "set", "tasks": tasks})).is_error);
    todo
}

#[test]
fn expanded_blocker_keeps_the_full_reason_with_bounded_width() {
    let mut todo = list(json!([{"content":"Implement"}]));
    let reason = "Wrong interpreter. Use Python 3.14 with uv run, preserve the code, then rerun the existing checks. END-OF-REASON";
    todo.call(&json!({"op":"block","reason":reason}));
    let cards = changes(&TodoList::new(), &todo);
    let expanded = card_rows(&cards, 30, &Theme::dark(), true);
    assert!(expanded.iter().all(|line| line.width() <= 30));
    assert!(plain(&expanded).join("").contains("END-OF-REASON"));
    assert!(todo.render().contains(reason));
    assert!(
        !plain(&card_rows(&cards, 30, &Theme::dark(), false))
            .join("")
            .contains("END-OF-REASON")
    );
}

#[test]
fn hidden_when_nothing_is_open_or_too_small() {
    let theme = Theme::dark();
    assert!(rows(&TodoList::new(), 80, &theme, None, 10).is_empty());
    assert!(
        rows(
            &list(json!([{"content": "a", "status": "completed"}])),
            80,
            &theme,
            None,
            10
        )
        .is_empty()
    );
    assert!(
        rows(
            &list(json!([{"content": "a"}])),
            80,
            &theme,
            None,
            MIN_ROWS - 1
        )
        .is_empty()
    );
}

#[test]
fn shows_progress_status_glyphs_and_blockers() {
    let theme = Theme::dark();
    let mut todo = list(
        json!([{"content": "read", "status": "completed"}, {"content": "fix"}, {"content": "ship"}]),
    );
    todo.call(&json!({"op": "block", "index": 3, "reason": "no creds"}));
    let text = plain(&rows(&todo, 60, &theme, Some("⠋"), 10));
    assert_eq!(
        text,
        [
            "TASKS  1/3 done · 1 blocked — /todo details",
            "  ✓ read",
            "  ⠋ fix",
            "  ⚠ ship · no creds"
        ]
    );
}

#[test]
fn long_lists_window_around_the_active_task_and_fit_the_width() {
    let theme = Theme::dark();
    let tasks: Vec<_> = (1..=12).map(|i| json!({"content": format!("task {i} with a fairly long description"), "status": if i < 7 { "completed" } else { "pending" }})).collect();
    let todo = list(json!(tasks));
    let rows = rows(&todo, 30, &theme, None, 6);
    let text = plain(&rows);
    assert_eq!(text.len(), 6, "{text:?}");
    assert!(
        text[1].contains("2 earlier") && text[3].contains("task 7") && text[5].contains("4 more"),
        "{text:?}"
    );
    assert!(rows.iter().all(|r| text::line_width(r) <= 30));
}

#[test]
fn phases_become_headings() {
    let theme = Theme::dark();
    let todo = list(
        json!([{"content": "read", "phase": "Investigate"}, {"content": "fix", "phase": "Implement"}]),
    );
    let text = plain(&rows(&todo, 60, &theme, None, 10));
    assert_eq!(
        text,
        [
            "TASKS  0/2 done",
            "  Investigate",
            "    ● read",
            "  Implement",
            "    ○ fix"
        ]
    );
}

#[test]
fn a_card_flags_what_the_call_changed_and_strikes_closed_steps() {
    let theme = Theme::dark();
    let before = list(json!([{"content": "read"}, {"content": "fix"}, {"content": "test"}]));
    assert!(
        changes(&TodoList::new(), &before)
            .iter()
            .all(|(_, changed)| !changed),
        "a first plan flags nothing"
    );
    let mut after = before.clone();
    after.call(&json!({"op": "done"}));
    let flagged = changes(&before, &after);
    assert_eq!(
        flagged.iter().map(|(_, c)| *c).collect::<Vec<_>>(),
        [true, true, false],
        "closed step and the step it started"
    );
    let rows = card_rows(&flagged, 60, &theme, false);
    assert_eq!(plain(&rows), ["  ✓ read", "  ● fix", "  ○ test"]);
    let read = rows[0].spans.iter().find(|s| s.content == "read").unwrap();
    assert!(
        read.style
            .add_modifier
            .contains(Modifier::CROSSED_OUT | Modifier::BOLD),
        "the closed step is struck through and emphasized"
    );
    let test = rows[2].spans.iter().find(|s| s.content == "test").unwrap();
    assert!(!test.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn long_cards_collapse_to_the_change_unless_expanded() {
    let theme = Theme::dark();
    let tasks: Vec<_> = (1..=30).map(|i| json!({"content": format!("t{i}"), "status": if i < 20 { "completed" } else { "pending" }})).collect();
    let before = list(json!(tasks));
    let mut after = before.clone();
    after.call(&json!({"op": "done"}));
    let text = plain(&card_rows(&changes(&before, &after), 60, &theme, false));
    assert_eq!(
        text,
        [
            "    … 1 more",
            "  ✓ t19",
            "  ✓ t20",
            "  ● t21",
            "  ○ t22",
            "    … 8 more"
        ]
    );
    assert_eq!(
        card_rows(&changes(&before, &after), 60, &theme, true).len(),
        13
    );
}
