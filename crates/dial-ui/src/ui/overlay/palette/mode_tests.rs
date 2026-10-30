use super::super::settings::testkit::*;
use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn plain(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn entries() -> Vec<PaletteEntry> {
    [("rename", "Name this conversation", Some("<title>"))]
        .into_iter()
        .map(|(name, description, args)| PaletteEntry {
            command: CommandEntry {
                name: name.into(),
                description: description.into(),
                kind: CommandKind::Builtin,
                args_hint: args.map(str::to_owned),
            },
            during_turn: "Runs after the current turn finishes.",
        })
        .collect()
}

fn item(value: &str, current: bool) -> ChoiceItem {
    ChoiceItem {
        value: value.into(),
        label: value.into(),
        detail: format!("{value} detail"),
        current,
    }
}

fn rendered(palette: &mut PaletteOverlay, width: u16) -> Vec<String> {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    palette
        .compose(width, 24, &ctx)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn footer_lists_only_the_keys_each_mode_uses() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    let footer = rendered(&mut palette, 100).pop().unwrap();
    assert!(
        footer.contains("Tab category") && footer.contains("choose") && footer.contains("close"),
        "{footer}"
    );
    palette.handle(&Event::Paste("rename".into()), &keys);
    palette.handle(&plain(KeyCode::Enter), &keys);
    let footer = rendered(&mut palette, 100).pop().unwrap();
    assert!(
        footer.contains("back") && !footer.contains("Tab") && !footer.contains("arguments"),
        "{footer}"
    );
}

#[test]
fn empty_required_argument_says_which_value_is_missing() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("rename".into()), &keys);
    palette.handle(&plain(KeyCode::Enter), &keys);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::None
    ));
    assert!(
        rendered(&mut palette, 100)
            .iter()
            .any(|l| l.contains("Enter a value for <title>")),
    );
}

#[test]
fn picker_applies_the_selected_value_and_marks_the_current_one() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Approval".into(),
        command: "approval".into(),
        verb: "",
        remove: None,
        items: vec![item("ask", false), item("write", true), item("auto", false)],
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    assert!(
        rendered(&mut palette, 100)
            .iter()
            .any(|l| l.contains("current"))
    );
    // The cursor starts on the current value, so Enter applies it without moving first.
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/approval write"
    ));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Esc), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn picker_with_remove_edits_or_removes_the_selected_row() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Queued messages".into(),
        command: "queue".into(),
        verb: "edit",
        remove: Some("remove"),
        items: vec![item("1", false), item("2", false)],
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    palette.handle(&plain(KeyCode::Down), &keys);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/queue edit 2"
    ));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Delete), &keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Delete), &keys),
        OverlayEvent::Action(action) if action == "command:/queue remove 2"
    ));
}

#[test]
fn remove_needs_a_second_press_on_the_same_row_and_names_the_item() {
    let keys = Keymap::defaults();
    for (code, key_name) in [(KeyCode::Delete, "Delete"), (KeyCode::Char('x'), "x")] {
        let spec = ChoiceSpec {
            title: "Queued messages".into(),
            command: "queue".into(),
            verb: "edit",
            remove: Some("remove"),
            items: vec![item("1", false), item("2", false)],
            ..Default::default()
        };
        let mut palette = PaletteOverlay::choice(spec);
        palette.handle(&plain(KeyCode::Down), &keys);
        assert!(matches!(
            palette.handle(&plain(code), &keys),
            OverlayEvent::None
        ));
        let shown = rendered(&mut palette, 100).join("\n");
        assert!(
            shown.contains(&format!("Press {key_name} again to remove 2")),
            "{shown}"
        );
        assert!(matches!(
            palette.handle(&plain(code), &keys),
            OverlayEvent::Action(action) if action == "command:/queue remove 2"
        ));
    }
}

#[test]
fn backspace_never_removes_and_any_other_key_disarms_a_pending_remove() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Queued messages".into(),
        command: "queue".into(),
        verb: "edit",
        remove: Some("remove"),
        items: vec![item("1", false), item("2", false)],
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    palette.handle(&plain(KeyCode::Down), &keys);
    for _ in 0..2 {
        assert!(matches!(
            palette.handle(&plain(KeyCode::Backspace), &keys),
            OverlayEvent::None
        ));
    }
    assert!(matches!(
        palette.handle(&plain(KeyCode::Delete), &keys),
        OverlayEvent::None
    ));
    palette.handle(&plain(KeyCode::Up), &keys);
    palette.handle(&plain(KeyCode::Down), &keys);
    assert!(
        matches!(
            palette.handle(&plain(KeyCode::Delete), &keys),
            OverlayEvent::None
        ),
        "moving away and back disarmed the first press"
    );
    let footer = rendered(&mut palette, 100).pop().unwrap();
    assert!(footer.contains("remove (press twice)"), "{footer}");
}

#[test]
fn long_details_end_in_an_ellipsis_instead_of_disappearing() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(
        vec![PaletteEntry {
            command: CommandEntry {
                name: "verbose".into(),
                description: "word ".repeat(60),
                kind: CommandKind::Custom,
                args_hint: None,
            },
            during_turn: "later",
        }],
        false,
    );
    palette.handle(&Event::Paste("verbose".into()), &keys);
    let lines = rendered(&mut palette, 40);
    assert!(lines.iter().any(|l| l.contains('…')), "{lines:?}");
}

#[test]
fn empty_picker_search_names_choices_and_drops_selection_keys() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Approval".into(),
        command: "approval".into(),
        verb: "",
        remove: None,
        items: vec![item("ask", false), item("write", true)],
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    let footer = rendered(&mut palette, 100).pop().unwrap();
    assert!(footer.contains("apply"), "{footer}");
    palette.handle(&Event::Paste("zzzz".into()), &keys);
    let lines = rendered(&mut palette, 100);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("No choices match. Esc clears the search.")),
        "{lines:?}"
    );
    let footer = lines.last().unwrap();
    assert!(
        !footer.contains("apply") && !footer.contains("Delete") && footer.contains("clear search"),
        "{footer}"
    );
}

#[test]
fn empty_action_search_offers_category_switch_instead_of_dead_enter() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("zzzz".into()), &keys);
    let lines = rendered(&mut palette, 100);
    assert!(
        lines.iter().any(|l| {
            l.contains("No actions match. Esc clears the search; Tab changes category.")
        }),
        "{lines:?}"
    );
    let footer = lines.last().unwrap();
    assert!(
        footer.contains("category") && !footer.contains("navigate") && !footer.contains("choose"),
        "{footer}"
    );
}

#[test]
fn search_header_names_its_scope_and_keeps_the_count_on_narrow_panels() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("name".into()), &keys);
    let lines = rendered(&mut palette, 24);
    let header = lines
        .iter()
        .find(|l| l.contains("Search ·"))
        .expect("search header");
    assert!(header.trim_end().ends_with("· 1/1"), "{header}");
}

#[test]
fn empty_search_header_drops_the_position_counter() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), false);
    palette.handle(&Event::Paste("zzzz".into()), &keys);
    let lines = rendered(&mut palette, 100);
    let header = lines.iter().find(|l| l.contains("Search ·")).unwrap();
    assert!(!header.contains("0/0"), "{header}");
}

#[test]
fn picker_copy_comes_from_the_spec_and_labels_use_the_free_width() {
    let long = "a long label that would have been cut at thirty-two columns END";
    let spec = ChoiceSpec {
        title: "Work plan".into(),
        command: "todo".into(),
        verb: "show",
        items: vec![ChoiceItem {
            value: "1".into(),
            label: long.into(),
            detail: "detail".into(),
            current: true,
        }],
        prompt: "Filter steps:".into(),
        summary: "1 step".into(),
        current_tag: Some(String::new()),
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    let lines = rendered(&mut palette, 120);
    assert!(
        lines.iter().any(|l| l.contains("Filter steps:")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.trim() == "1 step · 1/1"),
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains(long)), "{lines:?}");
    // An empty tag hides the redundant "current" marker.
    assert!(!lines.iter().any(|l| l.contains("current")), "{lines:?}");
}

#[test]
fn home_and_end_jump_to_the_list_ends_until_a_search_is_typed() {
    let keys = Keymap::defaults();
    let spec = ChoiceSpec {
        title: "Approval".into(),
        command: "approval".into(),
        items: vec![
            item("ask", false),
            item("write", false),
            item("auto", false),
        ],
        ..Default::default()
    };
    let mut palette = PaletteOverlay::choice(spec);
    palette.handle(&plain(KeyCode::End), &keys);
    assert_eq!(palette.cursor, 2);
    palette.handle(&plain(KeyCode::Home), &keys);
    assert_eq!(palette.cursor, 0);
    // While searching, Home/End move the caret in the search field instead.
    palette.handle(&Event::Paste("a".into()), &keys);
    palette.handle(&plain(KeyCode::Home), &keys);
    assert_eq!(palette.search.cursor, 0);
}

#[test]
fn narrow_panels_wrap_the_empty_message_and_drop_the_busy_note_before_overlap() {
    let keys = Keymap::defaults();
    let mut palette = PaletteOverlay::new(entries(), true);
    palette.handle(&Event::Paste("zzzz".into()), &keys);
    let lines = rendered(&mut palette, 38).join("\n");
    assert!(lines.contains("Tab changes category."), "{lines}");
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    for width in [12u16, 20, 38, 80] {
        let top = rows(&render(width, 12, |f| palette.draw(f, &ctx)))[0].clone();
        assert!(top.starts_with(" Actions"), "{width}: {top}");
        assert!(!top.contains("Actionsturn"), "{width}: {top}");
    }
}
