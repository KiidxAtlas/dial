use super::*;
use serde_json::json;

#[test]
fn recorded_references_never_look_like_passing_checks() {
    let view = render(Tab::Evidence, 120, 12);
    let shown = plain(&view).join("\n");
    assert!(
        !shown.contains('✓'),
        "references are observations, not check verdicts: {shown}"
    );
    let theme = Theme::builtin("dial-dark").unwrap();
    assert!(
        view.iter()
            .flat_map(|row| &row.spans)
            .all(|span| span.style.fg != theme.fg(Tok::Success).fg)
    );
    assert!(shown.contains("not verified"), "{shown}");
}

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn plan() -> TodoList {
    let mut todo = TodoList::new();
    assert!(
        !todo
            .call(&json!({"op": "set", "tasks": [
                {"content": "Map the renderer"},
                {"content": "Draw the cards"},
                {"content": "Write the docs"},
            ]}))
            .is_error
    );
    todo.call(&json!({"op": "done", "finding": "Paint owns the live region", "evidence": ["src/paint.rs:195"]}));
    todo
}

fn facts() -> Facts<'static> {
    Facts {
        approval: "ask",
        sandbox: "workspace",
        tab_key: "Ctrl+F",
        cycle_key: "Shift+Tab",
    }
}

fn render_todo(todo: &TodoList, tab: Tab, width: usize, max_rows: usize) -> Rows {
    rows(
        todo,
        &facts(),
        tab,
        width,
        max_rows,
        &Theme::builtin("dial-dark").unwrap(),
        None,
    )
}

fn render(tab: Tab, width: usize, max_rows: usize) -> Rows {
    render_todo(&plan(), tab, width, max_rows)
}

#[test]
fn narrow_short_or_finished_plans_yield_to_the_summary() {
    let theme = Theme::dark();
    assert!(render(Tab::Plan, COLLAPSED_MIN_WIDTH - 1, 20).is_empty());
    assert!(render(Tab::Plan, 120, MIN_ROWS - 1).is_empty());
    assert!(rows(&TodoList::new(), &facts(), Tab::Plan, 120, 20, &theme, None).is_empty());
}

#[test]
fn one_compact_card_lists_steps_beside_the_tab_strip_at_capped_width() {
    for width in [MIN_WIDTH, 120, 200] {
        let view = render(Tab::Plan, width, 12);
        assert!(view.len() >= MIN_ROWS && view.len() <= 12);
        for row in &view {
            assert_eq!(text::line_width(row), width.min(MAX_WIDTH), "{row:?}");
        }
        let text = plain(&view);
        assert!(text[0].starts_with('╭') && text[0].contains("tasks 1/3"));
        assert!(text.last().unwrap().starts_with('╰'));
        assert!(text.last().unwrap().contains("Ctrl+F next tab"));
        let joined = text.join("\n");
        for needle in [
            "✓ Map the renderer",
            "Draw the cards",
            "○ Write the docs",
            " plan ",
            " evidence ",
            " mode ",
        ] {
            assert!(joined.contains(needle), "{needle:?} in {joined}");
        }
    }
}

#[test]
fn the_card_is_content_sized_and_never_taller_than_allowed() {
    for width in [MIN_WIDTH, 120, 200] {
        // Three steps and the tab strip: the plan card is six rows at any width.
        assert_eq!(render(Tab::Plan, width, 20).len(), 6, "{width}");
        for tab in Tab::ALL {
            assert!(render(tab, width, 20).len() <= 8, "{tab:?} at {width}");
        }
    }
    assert_eq!(render(Tab::Plan, 120, MIN_ROWS).len(), MIN_ROWS);
}

#[test]
fn a_single_pending_step_renders_as_a_four_row_card_without_filler() {
    let mut todo = TodoList::new();
    todo.call(&json!({"op": "set", "tasks": [{"content": "Write the docs"}]}));
    let view = render_todo(&todo, Tab::Plan, 120, 6);
    let text = plain(&view);
    assert_eq!(view.len(), 4, "{text:?}");
    let joined = text.join("\n");
    assert!(joined.contains("Write the docs"), "{joined}");
    assert!(!joined.contains("no reason"), "{joined}");
}

#[test]
fn tabs_show_the_focused_step_evidence_and_approval_mode() {
    let plan = plain(&render(Tab::Plan, 120, 12)).join("\n");
    assert!(plan.contains("Draw the cards"), "{plan}");

    let evidence = plain(&render(Tab::Evidence, 120, 12)).join("\n");
    assert!(
        evidence.contains("src/paint.rs:195") && evidence.contains("not verified"),
        "{evidence}"
    );

    let mode = plain(&render(Tab::Mode, 120, 12)).join("\n");
    assert!(
        mode.contains("● Ask first")
            && mode.contains("sandbox workspace")
            && mode.contains("Shift+Tab cycles ask, write and read-only"),
        "{mode}"
    );
    assert_eq!(Tab::Plan.next(), Tab::Evidence);
    assert_eq!(Tab::Mode.next(), Tab::Plan);
}

#[test]
fn evidence_lists_newest_steps_first_and_counts_what_the_card_cuts() {
    let mut todo = TodoList::new();
    let tasks: Vec<_> = (0..4)
        .map(|i| json!({"content": format!("step {i}")}))
        .collect();
    todo.call(&json!({"op": "set", "tasks": tasks}));
    for i in 0..3 {
        todo.call(&json!({"op": "done", "evidence": [format!("ref {i}a"), format!("ref {i}b")]}));
    }
    // Plenty of room: every reference, newest step first.
    let all = plain(&render_todo(&todo, Tab::Evidence, 120, 40)).join("\n");
    assert!(!all.contains("more reference"), "{all}");
    assert!(
        all.find("step 2").unwrap() < all.find("step 0").unwrap(),
        "{all}"
    );
    // Eight rows: borders, tabs, the caveat, three rows and the overflow count.
    let view = render_todo(&todo, Tab::Evidence, 120, 8);
    let text = plain(&view);
    assert_eq!(view.len(), 8, "{text:?}");
    let joined = text.join("\n");
    assert!(
        joined.contains("step 2") && joined.contains("ref 2b"),
        "{joined}"
    );
    assert!(!joined.contains("step 0"), "{joined}");
    // Shown: step 2, ref 2a, ref 2b; the other four references are counted.
    assert!(joined.contains("+4 more references"), "{joined}");
}

#[test]
fn the_focused_step_stays_visible_at_every_card_height() {
    let mut todo = TodoList::new();
    let tasks: Vec<_> = (0..20)
        .map(|i| json!({"content": format!("step {i}")}))
        .collect();
    todo.call(&json!({"op": "set", "tasks": tasks}));
    for _ in 0..15 {
        todo.call(&json!({"op": "done"}));
    }
    for max_rows in [MIN_ROWS, 6, 8, 12] {
        let view = render_todo(&todo, Tab::Plan, 120, max_rows);
        let text = plain(&view);
        assert!(view.len() <= max_rows, "{max_rows}: {text:?}");
        assert!(text.join("\n").contains("step 15"), "{max_rows}: {text:?}");
    }
}

#[test]
fn medium_width_card_only_shows_current_and_next_steps() {
    for width in [60, 75, 89] {
        for tab in Tab::ALL {
            let view = render(tab, width, 12);
            let joined = plain(&view).join("\n");
            assert!(joined.contains("Draw the cards") && joined.contains("Write the docs"));
            assert!(!joined.contains("Map the renderer") && !joined.contains("evidence"));
            assert!(!joined.contains("next tab"));
            assert!(view.iter().all(|row| text::line_width(row) == width));
        }
    }
}
