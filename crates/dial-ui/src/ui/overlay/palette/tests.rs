use super::super::settings::testkit::*;
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn the_goal_header_and_count_use_ascii_separators_when_requested() {
    let mut rig = Rig::new();
    rig.theme = rig.theme.with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &|_, _| Line::default(),
    };
    let mut palette = PaletteOverlay::new(entries(), false);
    let shown = palette.compose(38, 16, &ctx);
    assert!(
        shown
            .iter()
            .flat_map(|row| &row.spans)
            .all(|span| span.content.is_ascii())
    );
}

#[test]
fn consistent_ux_advanced_commands_stay_searchable_outside_the_everyday_menu() {
    let mut entries = entries();
    entries.push(PaletteEntry {
        command: CommandEntry {
            name: "ratchet".into(),
            description: "Advanced verification loop".into(),
            kind: CommandKind::Builtin,
            args_hint: None,
        },
        during_turn: "Runs after this turn.",
    });
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries, false);
    assert!(palette.entry_rows().is_empty());
    palette.handle(&Event::Paste("ratchet".into()), &keys);
    assert_eq!(names(&palette).first(), Some(&"ratchet"));
}

fn entries() -> Vec<PaletteEntry> {
    [
        ("compute", "Stop unused processes and inspect memory", None),
        ("model", "Select a model", None),
        ("resume", "Resume a conversation", Some("[id]")),
        ("rename", "Name this conversation", Some("<title>")),
        ("undo", "Restore changes from the latest turn", None),
        ("settings", "Change preferences", None),
    ]
    .into_iter()
    .map(|(name, description, args)| PaletteEntry {
        command: CommandEntry {
            name: name.into(),
            description: description.into(),
            kind: CommandKind::Builtin,
            args_hint: args.map(str::to_owned),
        },
        during_turn: "Stops the current turn before running.",
    })
    .collect()
}
fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}
fn plain(code: KeyCode) -> Event {
    key(code, KeyModifiers::NONE)
}

/// Command names in the visible command rows, in display order.
fn names(palette: &PaletteOverlay) -> Vec<&str> {
    palette
        .entry_rows()
        .into_iter()
        .map(|index| palette.entries[index].command.name.as_str())
        .collect()
}

fn text_lines(palette: &mut PaletteOverlay) -> Vec<String> {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    palette
        .compose(100, 24, &ctx)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn goals_are_the_first_screen_and_open_their_commands() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    assert_eq!(
        palette.rows(),
        [
            Row::Goal(0),
            Row::Goal(1),
            Row::Goal(2),
            Row::Goal(3),
            Row::AllActions
        ]
    );
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert_eq!(palette.view, View::Goal(0));
    assert_eq!(names(&palette), ["resume", "model", "rename"]);
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/resume")
    );
}

#[test]
fn back_from_a_goal_returns_to_goals_without_running_anything() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&plain(KeyCode::Down), &keys);
    palette.handle(&plain(KeyCode::Enter), &keys);
    assert_eq!(palette.view, View::Goal(1));
    assert_eq!(names(&palette), ["undo"]);
    palette.handle(&Event::Paste("mod".into()), &keys);
    // Esc clears the search first and stays on the goal.
    assert!(matches!(
        palette.handle(&plain(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert!(palette.search.text.is_empty());
    assert_eq!(palette.view, View::Goal(1));
    // The next Esc returns to the goals with the cursor on the goal it came from.
    assert!(matches!(
        palette.handle(&plain(KeyCode::Esc), &keys),
        OverlayEvent::None
    ));
    assert_eq!(palette.view, View::Goals);
    assert_eq!(palette.cursor, 1);
    // Only Esc on the goal list closes the panel.
    assert!(matches!(
        palette.handle(&plain(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn goal_search_covers_every_registered_command() {
    let keys = Keymap::defaults();
    let mut source = entries();
    source.push(PaletteEntry {
        command: CommandEntry {
            name: "ratchet".into(),
            description: "Advanced verification loop".into(),
            kind: CommandKind::Builtin,
            args_hint: None,
        },
        during_turn: "Runs after this turn.",
    });
    let mut palette = PaletteOverlay::new(source, false);
    palette.handle(&plain(KeyCode::Enter), &keys);
    assert_eq!(palette.view, View::Goal(0));
    // "undo" belongs to another goal, and "ratchet" to none; both are found from here.
    palette.handle(&Event::Paste("undo".into()), &keys);
    assert_eq!(names(&palette).first(), Some(&"undo"));
    palette.handle(&plain(KeyCode::Esc), &keys);
    palette.handle(&Event::Paste("ratchet".into()), &keys);
    assert_eq!(names(&palette).first(), Some(&"ratchet"));
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/ratchet")
    );
}

#[test]
fn goal_with_no_matching_search_says_so_and_escape_leaves_in_steps() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&plain(KeyCode::Enter), &keys);
    palette.handle(&Event::Paste("zzzz".into()), &keys);
    assert!(palette.entry_rows().is_empty());
    assert!(
        text_lines(&mut palette)
            .iter()
            .any(|l| l.contains("No actions match")),
    );
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    palette.handle(&plain(KeyCode::Esc), &keys);
    assert_eq!(palette.view, View::Goal(0));
    palette.handle(&plain(KeyCode::Esc), &keys);
    assert_eq!(palette.view, View::Goals);
}

#[test]
fn busy_turn_still_lets_goals_open_and_leaves_command_policy_to_the_app() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), true);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert_eq!(palette.view, View::Goal(0));
    let lines = text_lines(&mut palette);
    assert!(lines.iter().any(|l| l.contains("Stops the current turn")));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/resume"
    ));
}

#[test]
fn all_actions_opens_the_category_tabs_and_tab_cycles_them() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    for _ in 0..4 {
        palette.handle(&plain(KeyCode::Down), &keys);
    }
    palette.handle(&plain(KeyCode::Enter), &keys);
    assert_eq!(palette.view, View::Tabs);
    assert_eq!(palette.group, 0);
    let all = names(&palette);
    assert_eq!(
        all.len(),
        entries().len(),
        "All actions must include advanced commands too: {all:?}"
    );
    assert!(all.contains(&"model") && all.contains(&"undo") && all.contains(&"compute"));
    palette.handle(&plain(KeyCode::Tab), &keys);
    assert_eq!(names(&palette), ["resume", "rename"]);
    palette.handle(&Event::Paste("compute".into()), &keys);
    assert_eq!(
        names(&palette),
        ["compute"],
        "typed search escapes the category scope"
    );
    palette.handle(&plain(KeyCode::Esc), &keys);
    palette.handle(&plain(KeyCode::Esc), &keys);
    assert_eq!(palette.view, View::Goals);
    assert_eq!(palette.cursor, GOALS.len());
}

#[test]
fn search_categories_and_default_actions_use_the_selected_command() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), true);
    palette.handle(&Event::Paste("MODEL".into()), &keys);
    assert_eq!(names(&palette).first(), Some(&"model"));
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/model")
    );
    palette.handle(&plain(KeyCode::Esc), &keys);
    // Tab from the goal list opens "All"; a second Tab moves to "Sessions".
    palette.handle(&plain(KeyCode::Tab), &keys);
    palette.handle(&plain(KeyCode::Tab), &keys);
    assert_eq!(names(&palette), ["resume", "rename"]);
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/resume")
    );
}

#[test]
fn arguments_require_explicit_confirmation_and_escape_keeps_search() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("rename".into()), &keys);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert!(palette.arguments.is_some());
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        palette.handle(&Event::Paste("日本語\nproject".into()), &keys),
        OverlayEvent::None
    ));
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/rename 日本語 project")
    );
    palette.handle(&plain(KeyCode::Esc), &keys);
    assert_eq!(palette.search.text, "rename");
    assert!(palette.arguments.is_none());
    palette.handle(&plain(KeyCode::Esc), &keys);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn optional_arguments_are_available_without_replacing_the_default_action() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("resume".into()), &keys);
    palette.handle(&key(KeyCode::Enter, KeyModifiers::SHIFT), &keys);
    assert!(palette.arguments.is_some());
    palette.handle(&Event::Paste("abc123".into()), &keys);
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/resume abc123")
    );
}

#[test]
fn unmatched_and_untrusted_entries_cannot_dispatch() {
    let keys = Keymap::defaults();
    let mut source = entries();
    source.push(PaletteEntry {
        command: CommandEntry {
            name: "bad\x1bname".into(),
            description: "bad".into(),
            kind: CommandKind::Custom,
            args_hint: None,
        },
        during_turn: "later",
    });
    let mut palette = PaletteOverlay::new(source, false);
    assert_eq!(palette.entries.len(), entries().len());
    palette.handle(&Event::Paste("unmatchable-command".into()), &keys);
    assert!(palette.entry_rows().is_empty());
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
}

#[test]
fn layout_preserves_details_controls_and_fits_small_terminals() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), true);
    for (w, h) in [(0, 0), (1, 1), (2, 2), (8, 4), (30, 10), (100, 24)] {
        for mode in [false, true] {
            palette.arguments = mode.then(|| (3, Editor::default()));
            let rows = palette.compose(w, h, &ctx);
            assert!(rows.len() <= usize::from(h));
            assert!(
                rows.iter()
                    .all(|row| text::line_width(row) <= usize::from(w))
            );
            render(w, h, |frame| palette.draw(frame, &ctx));
        }
    }
    palette.arguments = None;
    let lines = text_lines(&mut palette);
    assert!(lines.iter().any(|l| l.contains("Start or continue work")));
    assert!(lines.iter().any(|l| l.contains("All actions")));
    assert!(lines.last().unwrap().contains("close"));
    palette.handle(&plain(KeyCode::Enter), &keys);
    let lines = text_lines(&mut palette);
    assert!(lines.iter().any(|l| l.contains("Stops the current turn")));
    assert!(lines.last().unwrap().contains("back"));
}

#[test]
fn task_labels_are_searchable_without_changing_command_dispatch() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("Choose a model".into()), &keys);
    assert_eq!(palette.entry_rows().len(), 1);
    assert!(matches!(palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/model"));
    let custom = CommandEntry {
        name: "model".into(),
        description: "My workflow".into(),
        kind: CommandKind::Custom,
        args_hint: None,
    };
    assert_eq!(action_label(&custom), "model");
}

#[test]
fn manage_goal_lists_todo_alongside_work_commands() {
    let keys = Keymap::defaults();
    let mut source = entries();
    source.push(PaletteEntry {
        command: CommandEntry {
            name: "todo".into(),
            description: "Browse the task plan".into(),
            kind: CommandKind::Builtin,
            args_hint: None,
        },
        during_turn: "Runs after this turn.",
    });
    let mut palette = PaletteOverlay::new(source, false);
    palette.handle(&plain(KeyCode::Down), &keys);
    palette.handle(&plain(KeyCode::Down), &keys);
    palette.handle(&plain(KeyCode::Enter), &keys);
    assert_eq!(palette.view, View::Goal(2));
    assert_eq!(names(&palette), ["todo", "compute"]);
}

#[test]
fn short_panels_keep_cancel_visible_and_clicks_match_drawn_rows() {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    for picker in [false, true] {
        let mut palette = if picker {
            PaletteOverlay::choice(ChoiceSpec {
                title: "Choice".into(),
                command: "theme".into(),
                verb: "",
                remove: None,
                items: ["dark", "light"]
                    .into_iter()
                    .map(|value| ChoiceItem {
                        value: value.into(),
                        label: value.into(),
                        detail: String::new(),
                        current: false,
                    })
                    .collect(),
                ..Default::default()
            })
        } else {
            PaletteOverlay::new(entries(), false)
        };
        for height in [4, 6, 8, 12] {
            let rows = palette.compose(40, height, &ctx);
            let footer: String = rows
                .last()
                .unwrap()
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert!(footer.contains("Esc"), "{footer}");
            assert_eq!(rows.len(), usize::from(height));
        }
        let buffer = render(80, 24, |frame| palette.draw(frame, &ctx));
        let list_top = palette.list_top();
        let first_row = rows(&buffer)[list_top].clone();
        if picker {
            assert!(first_row.contains("dark"));
            // The first click on the cursor row runs it, like Enter.
            assert!(matches!(
                palette.click(list_top as u16),
                OverlayEvent::Action(action) if action == "command:/theme dark"
            ));
        } else {
            assert!(first_row.contains("Start or continue work"), "{first_row}");
            // Clicking an unselected goal only selects it; the next click opens it.
            assert!(matches!(
                palette.click(list_top as u16 + 1),
                OverlayEvent::None
            ));
            assert_eq!(palette.view, View::Goals);
            assert_eq!(palette.cursor, 1);
            assert!(matches!(
                palette.click(list_top as u16 + 1),
                OverlayEvent::None
            ));
            assert_eq!(palette.view, View::Goal(1));
            assert_eq!(names(&palette), ["undo"]);
        }
    }
}

#[test]
fn picker_opens_on_the_current_value_and_click_targets_it() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Approval".into(),
        command: "approval".into(),
        verb: "",
        remove: None,
        items: ["ask", "write", "auto"]
            .into_iter()
            .map(|value| ChoiceItem {
                value: value.into(),
                label: value.into(),
                detail: String::new(),
                current: value == "write",
            })
            .collect(),
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    assert_eq!(palette.cursor, 1);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/approval write"
    ));
    let list_top = palette.list_top() as u16;
    assert!(matches!(
        palette.click(list_top + 1),
        OverlayEvent::Action(action) if action == "command:/approval write"
    ));
}
