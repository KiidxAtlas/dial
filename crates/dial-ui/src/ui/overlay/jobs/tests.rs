use super::*;
use crossterm::event::KeyEventKind;

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, modifiers, KeyEventKind::Press)
}

fn row(name: &str, running: bool) -> JobRow {
    JobRow {
        name: name.into(),
        command: format!("run {name}"),
        status: if running { "running" } else { "exited 0" }.into(),
        running,
        terminal: true,
        mode: "session".into(),
    }
}

#[test]
fn keys_encode_like_a_terminal() {
    assert_eq!(
        key_bytes(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(vec![3])
    );
    assert_eq!(
        key_bytes(&key(KeyCode::Char('x'), KeyModifiers::ALT)),
        Some(vec![0x1b, b'x'])
    );
    assert_eq!(
        key_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)),
        Some(vec![b'\r'])
    );
    assert_eq!(
        key_bytes(&key(KeyCode::Up, KeyModifiers::NONE)),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        key_bytes(&key(KeyCode::F(5), KeyModifiers::NONE)),
        Some(b"\x1b[15~".to_vec())
    );
    assert_eq!(
        key_bytes(&key(KeyCode::Char('é'), KeyModifiers::NONE)),
        Some("é".as_bytes().to_vec())
    );
}

#[test]
fn attach_forwards_keys_and_detach_returns_to_the_list() {
    let keys = Keymap::defaults();
    let mut overlay = JobsOverlay::new();
    overlay.set_jobs(vec![row("server", true), row("watch", true)]);
    overlay.handle(&Event::Key(key(KeyCode::Down, KeyModifiers::NONE)), &keys);
    overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
    assert_eq!(overlay.attached(), Some("watch"));
    // Esc and Ctrl+C belong to the process while attached; they never close the panel.
    match overlay.handle(&Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)), &keys) {
        OverlayEvent::JobInput { name, bytes } => {
            assert_eq!(name, "watch");
            assert_eq!(bytes, vec![0x1b]);
        }
        other => panic!("expected input, got {other:?}"),
    }
    match overlay.handle(&Event::Paste("a\nb".into()), &keys) {
        OverlayEvent::JobInput { bytes, .. } => assert_eq!(bytes, b"a\rb".to_vec()),
        other => panic!("expected input, got {other:?}"),
    }
    overlay.handle(
        &Event::Key(key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
        &keys,
    );
    assert_eq!(overlay.attached(), None);
    assert!(matches!(
        overlay.handle(&Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)), &keys),
        OverlayEvent::Close
    ));
}

#[test]
fn a_vanished_job_detaches_and_the_selection_follows_names() {
    let keys = Keymap::defaults();
    let mut overlay = JobsOverlay::new();
    overlay.set_jobs(vec![row("a", true), row("b", true)]);
    overlay.handle(&Event::Key(key(KeyCode::Down, KeyModifiers::NONE)), &keys);
    overlay.set_jobs(vec![row("new", true), row("a", true), row("b", false)]);
    overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
    assert_eq!(overlay.attached(), Some("b"));
    overlay.set_jobs(vec![row("a", true)]);
    assert_eq!(overlay.attached(), None);
}

#[test]
fn terminal_size_leaves_room_for_the_frame() {
    assert_eq!(JobsOverlay::terminal_size(80, 24), (78, 20));
    assert_eq!(JobsOverlay::terminal_size(1, 1), (1, 1));
}
