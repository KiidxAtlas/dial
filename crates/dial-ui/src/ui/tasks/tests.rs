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
fn live_summary_prioritizes_current_work_and_blockers_over_closed_history() {
    let mut todo = list(json!([
        {"content":"read", "status":"completed"},
        {"content":"fix", "phase":"Implement"},
        {"content":"ship"},
        {"content":"verify"}
    ]));
    todo.call(&json!({"op":"block", "index":3, "reason":"no creds"}));
    let shown = plain(&rows(&todo, 100, &Theme::dark(), Some("⠋"), 10));
    assert_eq!(shown.len(), 4, "{shown:?}");
    assert!(shown[0].contains("1/4") && shown[0].contains("1 blocked"));
    assert!(shown[1].contains("fix") && shown[1].contains("Now"));
    assert!(shown[2].contains("ship") && shown[2].contains("no creds"));
    assert!(shown[3].contains("verify") && shown[3].contains("Next"));
    assert!(!shown[1..].iter().any(|line| line.contains("read")));
    assert!(shown[0].contains("/todo"));
}

#[test]
fn summary_budget_keeps_focus_instead_of_repeating_the_entire_plan() {
    let tasks: Vec<_> = (1..=12).map(|i| json!({"content":format!("task {i}"), "status":if i < 7 {"completed"} else {"pending"}})).collect();
    let todo = list(json!(tasks));
    for theme in [
        Theme::dark(),
        Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
    ] {
        for width in [0, 11, 12, 30, 80] {
            for budget in 0..=10 {
                let lines = rows(&todo, width, &theme, None, budget);
                assert!(lines.len() <= budget.min(4));
                assert!(lines.iter().all(|row| text::line_width(row) <= width));
                if width >= 30 && budget >= MIN_ROWS {
                    assert!(plain(&lines)[1].contains("task 7"));
                }
                if theme.ascii() {
                    assert!(plain(&lines).join("").is_ascii());
                }
            }
        }
    }
}

#[test]
fn inspector_retains_stable_task_numbers_phases_findings_and_evidence_without_mutation() {
    let mut todo = list(json!([
        {"content":"read", "phase":"Investigate"},
        {"content":"fix", "phase":"Implement"},
        {"content":"ship"}
    ]));
    todo.call(&json!({"op":"done", "finding":"Cause identified", "evidence":["src/main.rs:42"]}));
    todo.call(&json!({"op":"block", "index":3, "reason":"Full blocked reason END-OF-REASON"}));
    let before = todo.clone();
    let spec = plan_picker(&todo).unwrap();
    assert_eq!(
        spec.items
            .iter()
            .map(|item| item.value.as_str())
            .collect::<Vec<_>>(),
        ["2", "3", "1"]
    );
    assert!(spec.items[0].current);
    assert!(spec.items[0].detail.contains("Phase: Implement"));
    assert!(spec.items[1].detail.contains("END-OF-REASON"));
    assert!(spec.items[2].detail.contains("Cause identified"));
    assert!(spec.items[2].detail.contains("src/main.rs:42"));
    assert_eq!(spec.command, "todo");
    assert_eq!(spec.verb, "show");
    assert!(spec.remove.is_none());
    assert_eq!(todo, before);
    assert!(plan_picker(&TodoList::new()).is_none());
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

#[test]
fn filtering_the_plan_opens_the_original_task_number_not_the_visible_row_number() {
    use crate::ui::{
        keys::Keymap,
        overlay::{OverlayEvent, palette::PaletteOverlay},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut todo =
        list(json!([{"content":"investigate"},{"content":"implement"},{"content":"release"}]));
    todo.call(&json!({"op":"block","index":3,"reason":"credentials unavailable"}));
    let before = todo.clone();
    let mut picker = PaletteOverlay::choice(plan_picker(&todo).unwrap());
    let keys = Keymap::defaults();
    picker.handle(&Event::Paste("credentials".into()), &keys);
    let action = picker.handle(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &keys,
    );
    assert!(matches!(action, OverlayEvent::Action(command) if command == "command:/todo show 3"));
    assert_eq!(todo, before);
}

#[test]
fn plan_picker_summarizes_the_plan_and_does_not_repeat_the_status() {
    let mut todo = list(json!([{"content":"a"},{"content":"b"},{"content":"c"},{"content":"d"}]));
    todo.call(&json!({"op":"done"}));
    todo.call(&json!({"op":"block", "index":3, "reason":"waiting"}));
    let spec = plan_picker(&todo).unwrap();
    assert_eq!(spec.summary, "4 steps, 1 done, 1 blocked");
    assert_eq!(spec.prompt, "Filter steps:");
    let working = &spec.items[0];
    assert!(working.label.contains("Working: b"), "{}", working.label);
    assert!(!working.detail.contains("Status:"), "{}", working.detail);
    // The standalone step view keeps the status, since it has no label beside it.
    assert!(step_detail(&todo.tasks()[1]).contains("Status: Working"));
}
