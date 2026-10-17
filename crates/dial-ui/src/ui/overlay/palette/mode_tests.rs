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
        footer.contains("Tab category") && footer.contains("arguments") && footer.contains("close"),
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
    };
    let mut palette = PaletteOverlay::choice(spec);
    assert!(
        rendered(&mut palette, 100)
            .iter()
            .any(|l| l.contains("current"))
    );
    palette.handle(&plain(KeyCode::Down), &keys);
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
    };
    let mut palette = PaletteOverlay::choice(spec);
    palette.handle(&plain(KeyCode::Down), &keys);
    assert!(matches!(
        palette.handle(&plain(KeyCode::Enter), &keys),
        OverlayEvent::Action(action) if action == "command:/queue edit 2"
    ));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Delete), &keys),
        OverlayEvent::Action(action) if action == "command:/queue remove 2"
    ));
    assert!(matches!(
        palette.handle(&plain(KeyCode::Char('x')), &keys),
        OverlayEvent::Action(action) if action == "command:/queue remove 2"
    ));
}

#[test]
fn long_details_end_in_an_ellipsis_instead_of_disappearing() {
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
    let lines = rendered(&mut palette, 40);
    assert!(lines.iter().any(|l| l.contains('…')), "{lines:?}");
}
