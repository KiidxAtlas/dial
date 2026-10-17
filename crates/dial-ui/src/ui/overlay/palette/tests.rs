use super::super::settings::testkit::*;
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    let mut palette = PaletteOverlay::new(entries, false);
    assert!(
        !palette
            .rows()
            .iter()
            .any(|&i| palette.entries[i].command.name == "ratchet")
    );
    palette.handle(&Event::Paste("ratchet".into()), &Keymap::defaults());
    assert_eq!(palette.entries[palette.rows()[0]].command.name, "ratchet");
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

#[test]
fn search_categories_and_default_actions_use_the_selected_command() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), true);
    palette.handle(&Event::Paste("MODEL".into()), &keys);
    assert_eq!(palette.entries[palette.rows()[0]].command.name, "model");
    assert!(
        matches!(palette.handle(&plain(KeyCode::Enter), &keys), OverlayEvent::Action(action) if action == "command:/model")
    );
    palette.handle(&plain(KeyCode::Esc), &keys);
    palette.handle(&plain(KeyCode::Tab), &keys);
    let rows: Vec<_> = palette
        .rows()
        .iter()
        .map(|&i| palette.entries[i].command.name.as_str())
        .collect();
    assert_eq!(rows, ["resume", "rename"]);
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
    assert!(palette.rows().is_empty());
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
    let lines = palette.compose(100, 24, &ctx);
    let lines: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(lines.iter().any(|l| l.contains("Stops the current turn")));
    assert!(lines.last().unwrap().contains("close"));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("All") && l.contains("Extensions"))
    );
}

#[test]
fn task_labels_are_searchable_without_changing_command_dispatch() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("Choose a model".into()), &keys);
    assert_eq!(palette.rows().len(), 1);
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
        let first_row = rows(&buffer)[palette.list_top()].clone();
        assert!(first_row.contains(if picker { "dark" } else { "Choose a model" }));
        let expected = if picker {
            "command:/theme dark"
        } else {
            "command:/model"
        };
        assert!(
            matches!(palette.click(palette.list_top() as u16), OverlayEvent::Action(action) if action == expected)
        );
    }
}
