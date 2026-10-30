use super::*;
use crate::ui::overlay::settings::testkit::{Rig, ch, key, render, rows, style_of, typed};
use crate::ui::theme::{Glyphs, Theme};
use ratatui::buffer::Buffer;

const OWN: &str = "0123456789abcdef0123456789abcdef";
const PEER_A: &str = "aaaaaaaa11111111aaaaaaaa11111111";
const PEER_B: &str = "bbbbbbbb22222222bbbbbbbb22222222";

fn claim(path: &str, overlaps: bool) -> Claim {
    Claim {
        path: path.into(),
        overlaps,
    }
}

fn board() -> PeersView {
    PeersView {
        own: PeerRow {
            id: OWN.into(),
            phase: "working".into(),
            summary: "refactor the parser".into(),
            claims: vec![claim("src/parser", true), claim("docs", false)],
        },
        peers: vec![
            PeerRow {
                id: PEER_A.into(),
                phase: "working".into(),
                summary: "update the client".into(),
                claims: vec![claim("src/parser/lexer.rs", true)],
            },
            PeerRow {
                id: PEER_B.into(),
                phase: "idle".into(),
                summary: String::new(),
                claims: Vec::new(),
            },
        ],
        queued: 2,
    }
}

fn draw(overlay: &mut PeersOverlay, rig: &Rig, w: u16, h: u16) -> Buffer {
    let preview = |_: &crate::llm::model_config::UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &preview,
    };
    render(w, h, |frame| overlay.draw(frame, &ctx))
}

fn shown(overlay: &mut PeersOverlay, rig: &Rig, w: u16, h: u16) -> String {
    rows(&draw(overlay, rig, w, h)).join("\n")
}

fn action(event: OverlayEvent) -> (PeerAction, Option<String>) {
    match event {
        OverlayEvent::Action(action) => PeerAction::parse(&action).expect("a peers action"),
        other => panic!("expected an action, got {other:?}"),
    }
}

#[test]
fn board_shows_sessions_phases_summaries_claims_and_inbox() {
    let rig = Rig::new();
    let mut overlay = PeersOverlay::new(board());
    let screen = shown(&mut overlay, &rig, 90, 24);
    for needle in [
        "Project peers",
        "2 peers",
        "1 overlapping",
        "2 unread",
        "This session",
        "01234567",
        "refactor the parser",
        "src/parser",
        "docs",
        "inbox  2 unread messages",
        "Other sessions in this checkout",
        "aaaaaaaa",
        "update the client",
        "src/parser/lexer.rs",
        "bbbbbbbb",
        "idle",
        "no task summary",
        "no claims",
        "r refresh",
        "c clear my claims",
        "close",
    ] {
        assert!(screen.contains(needle), "missing {needle:?}:\n{screen}");
    }
    // Only the short id is shown.
    assert!(!screen.contains(PEER_A), "{screen}");
}

#[test]
fn overlapping_claims_are_highlighted_and_others_are_not() {
    let rig = Rig::new();
    let mut overlay = PeersOverlay::new(board());
    let buffer = draw(&mut overlay, &rig, 90, 24);
    let warning = rig.theme.fg(Tok::Warning).fg;
    assert_eq!(style_of(&buffer, "lexer.rs").unwrap().fg, warning);
    assert_eq!(style_of(&buffer, "src/parser ").unwrap().fg, warning);
    assert_ne!(style_of(&buffer, "docs").unwrap().fg, warning);
    let screen = rows(&buffer).join("\n");
    assert!(screen.contains("⚠ src/parser/lexer.rs"), "{screen}");
    assert!(!screen.contains("⚠ docs"), "{screen}");
}

#[test]
fn empty_board_explains_itself_and_offers_no_message_or_clear() {
    let rig = Rig::new();
    let mut overlay = PeersOverlay::new(PeersView {
        own: PeerRow {
            id: OWN.into(),
            phase: "idle".into(),
            ..PeerRow::default()
        },
        ..PeersView::default()
    });
    let screen = shown(&mut overlay, &rig, 60, 16);
    assert!(screen.contains("0 peers"), "{screen}");
    assert!(screen.contains("No other Dial sessions"), "{screen}");
    assert!(screen.contains("inbox  empty"), "{screen}");
    assert!(!screen.contains("unread"), "{screen}");
    assert!(!screen.contains("message"), "{screen}");
    assert!(!screen.contains("clear my claims"), "{screen}");
    // Neither Enter nor c asks the controller for anything.
    assert!(matches!(
        overlay.handle(&key(KeyCode::Enter), &rig.keys),
        OverlayEvent::None
    ));
    assert!(matches!(
        overlay.handle(&ch('c'), &rig.keys),
        OverlayEvent::None
    ));
    assert!(shown(&mut overlay, &rig, 60, 16).contains("no claims to clear"));
}

#[test]
fn keys_close_refresh_clear_and_carry_the_selection() {
    let rig = Rig::new();
    let mut overlay = PeersOverlay::new(board());
    overlay.handle(&key(KeyCode::Down), &rig.keys);
    assert_eq!(overlay.selected(), Some(PEER_B));
    assert_eq!(
        action(overlay.handle(&ch('r'), &rig.keys)),
        (PeerAction::Refresh, Some(PEER_B.to_owned()))
    );
    assert_eq!(
        action(overlay.handle(&ch('c'), &rig.keys)),
        (PeerAction::Clear, Some(PEER_B.to_owned()))
    );
    overlay.handle(&key(KeyCode::Home), &rig.keys);
    assert_eq!(overlay.selected(), Some(PEER_A));
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::Close
    ));
}

#[test]
fn compose_sends_to_the_selected_peer_and_esc_only_cancels_it() {
    let rig = Rig::new();
    let mut overlay = PeersOverlay::new(board());
    overlay.handle(&key(KeyCode::Enter), &rig.keys);
    // Typing goes to the field: r and c do not refresh or clear.
    for event in typed("keep the lexer API") {
        assert!(matches!(
            overlay.handle(&event, &rig.keys),
            OverlayEvent::None
        ));
    }
    let screen = shown(&mut overlay, &rig, 90, 24);
    assert!(
        screen.contains("To aaaaaaaa: keep the lexer API"),
        "{screen}"
    );
    assert!(screen.contains("Enter send"), "{screen}");
    assert_eq!(
        action(overlay.handle(&key(KeyCode::Enter), &rig.keys)),
        (
            PeerAction::Send {
                to: PEER_A.into(),
                text: "keep the lexer API".into()
            },
            Some(PEER_A.to_owned())
        )
    );
    // Esc leaves the field without closing the panel.
    overlay.handle(&ch('m'), &rig.keys);
    overlay.handle(&Event::Paste("two\nlines".into()), &rig.keys);
    assert!(shown(&mut overlay, &rig, 90, 24).contains("two lines"));
    assert!(matches!(
        overlay.handle(&key(KeyCode::Esc), &rig.keys),
        OverlayEvent::None
    ));
    assert!(!shown(&mut overlay, &rig, 90, 24).contains("Enter send"));
    // An empty message is refused in place.
    overlay.handle(&key(KeyCode::Enter), &rig.keys);
    assert!(matches!(
        overlay.handle(&key(KeyCode::Enter), &rig.keys),
        OverlayEvent::None
    ));
    assert!(shown(&mut overlay, &rig, 90, 24).contains("Type a message first"));
}

#[test]
fn actions_round_trip_and_foreign_actions_are_ignored() {
    for (action, selected) in [
        (PeerAction::Refresh, None),
        (PeerAction::Clear, Some(PEER_A.to_owned())),
        (
            PeerAction::Send {
                to: PEER_B.into(),
                text: "a: b\nc".into(),
            },
            Some(PEER_B.to_owned()),
        ),
    ] {
        let encoded = action.encode(selected.as_deref());
        assert_eq!(
            PeerAction::parse(&encoded),
            Some((action, selected)),
            "{encoded}"
        );
    }
    for foreign in [
        "open:models",
        "peers:",
        "peers:send:\nhi",
        "peers:send:abc\n  ",
    ] {
        assert_eq!(PeerAction::parse(foreign), None, "{foreign}");
    }
}

#[test]
fn reopening_keeps_the_selected_peer_while_it_is_live() {
    let mut overlay = PeersOverlay::new(board());
    overlay.select(PEER_B);
    assert_eq!(overlay.selected(), Some(PEER_B));
    overlay.select("gone");
    assert_eq!(overlay.selected(), Some(PEER_B));
}

#[test]
fn selection_scrolls_into_view_and_narrow_ascii_panels_stay_ascii() {
    let rig = Rig::with_theme(Theme::dark().with_glyphs(Glyphs::Ascii));
    let mut view = board();
    view.peers = (0..12)
        .map(|i| PeerRow {
            id: format!("{i:08x}{}", "f".repeat(24)),
            phase: "working".into(),
            summary: format!("task number {i} with a long summary that must be clipped"),
            claims: vec![claim(
                &format!("crates/very/long/path/to/module{i}/file.rs"),
                i % 2 == 0,
            )],
        })
        .collect();
    let mut overlay = PeersOverlay::new(view);
    overlay.handle(&key(KeyCode::End), &rig.keys);
    let screen = shown(&mut overlay, &rig, 40, 14);
    assert!(screen.contains("0000000b"), "last peer visible:\n{screen}");
    assert!(screen.contains("module11/file.rs"), "{screen}");
    assert!(
        !screen
            .chars()
            .any(|c| !c.is_ascii() && !"─│╭╮╰╯".contains(c)),
        "{screen}"
    );
    assert!(
        screen.contains("! crates") || screen.contains("! ..."),
        "{screen}"
    );
}
