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

fn draw_rows(
    overlay: &mut JobsOverlay,
    theme: &crate::ui::theme::Theme,
    w: u16,
    h: u16,
) -> Vec<String> {
    use super::super::settings::testkit::{render, rows};
    use crate::{llm::model_config::UiConfig, ui::status::StatusData};
    let keys = Keymap::defaults();
    let status = StatusData::default();
    let preview = |_: &UiConfig, _: u16| Line::default();
    let ctx = DrawCtx {
        theme,
        keys: &keys,
        status: &status,
        preview_status: &preview,
    };
    rows(&render(w, h, |frame| overlay.draw(frame, &ctx)))
}

#[test]
fn page_keys_step_by_a_screen_and_home_end_jump() {
    let keys = Keymap::defaults();
    let theme = crate::ui::theme::Theme::dark();
    let mut overlay = JobsOverlay::new();
    overlay.set_jobs((0..30).map(|i| row(&format!("job{i}"), true)).collect());
    draw_rows(&mut overlay, &theme, 80, 12);
    let page = overlay.list_h.get();
    assert!(page > 1 && page < 29);
    let press = |o: &mut JobsOverlay, code| {
        o.handle(&Event::Key(key(code, KeyModifiers::NONE)), &keys);
    };
    press(&mut overlay, KeyCode::PageDown);
    assert_eq!(overlay.cursor, page);
    press(&mut overlay, KeyCode::End);
    assert_eq!(overlay.cursor, 29);
    press(&mut overlay, KeyCode::PageUp);
    assert_eq!(overlay.cursor, 29 - page);
    press(&mut overlay, KeyCode::Home);
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn narrow_rows_end_long_commands_in_an_ellipsis_and_exited_jobs_offer_output() {
    let theme = crate::ui::theme::Theme::dark();
    let mut overlay = JobsOverlay::new();
    let mut long = row("a-very-long-server-name", false);
    long.command = "cargo watch -x 'run --release -- --port 8080 --verbose'".into();
    overlay.set_jobs(vec![long]);
    for width in [38u16, 60, 120] {
        let shown = draw_rows(&mut overlay, &theme, width, 12);
        let row = shown.iter().find(|l| l.contains("a-very")).unwrap();
        if width < 80 {
            assert!(row.trim_end().ends_with('…'), "{width}: {row}");
        }
        assert!(shown[0].contains("1 exited"), "{}", shown[0]);
        assert!(shown.join("\n").contains("view output"), "{shown:?}");
    }
    let ascii = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let shown = draw_rows(&mut overlay, &ascii, 60, 12).join("");
    assert!(
        !shown
            .chars()
            .any(|c| !c.is_ascii() && !"─│╭╮╰╯".contains(c)),
        "{shown}"
    );
}

#[test]
fn empty_list_explains_what_appears_here() {
    let theme = crate::ui::theme::Theme::dark();
    let mut overlay = JobsOverlay::new();
    let shown = draw_rows(&mut overlay, &theme, 38, 16).join("\n");
    assert!(shown.contains("none yet"), "{shown}");
    assert!(shown.contains("No background"), "{shown}");
    assert!(!shown.contains("attach"), "nothing to attach to: {shown}");
}

#[test]
fn pipe_and_exited_viewers_are_read_only_and_show_recent_output() {
    let keys = Keymap::defaults();
    let theme = crate::ui::theme::Theme::dark();
    for (terminal, running) in [(false, true), (false, false), (true, false)] {
        let mut overlay = JobsOverlay::new();
        let mut job = row("build", running);
        job.terminal = terminal;
        overlay.set_jobs(vec![job]);
        overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
        assert_eq!(overlay.attached(), Some("build"));
        for event in [
            Event::Key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Paste("do not send".into()),
        ] {
            assert!(matches!(overlay.handle(&event, &keys), OverlayEvent::None));
        }
        overlay.set_screen((0..100).map(|i| format!("output-{i}")).collect(), None);
        let shown = draw_rows(&mut overlay, &theme, 80, 12).join("\n");
        if !terminal {
            assert!(shown.contains("output-99"), "{shown}");
            assert!(!shown.contains("output-0 "), "{shown}");
        }
        assert!(!shown.contains("keys go to"), "{shown}");
        overlay.handle(&Event::Key(key(KeyCode::Esc, KeyModifiers::NONE)), &keys);
        assert_eq!(overlay.attached(), None);
    }
}

#[test]
fn pipe_output_scrolls_and_end_resumes_following() {
    let keys = Keymap::defaults();
    let theme = crate::ui::theme::Theme::dark();
    let mut overlay = JobsOverlay::new();
    let mut job = row("build", true);
    job.terminal = false;
    overlay.set_jobs(vec![job]);
    overlay.handle(&Event::Key(key(KeyCode::Enter, KeyModifiers::NONE)), &keys);
    overlay.set_screen((0..100).map(|i| format!("output-{i}")).collect(), None);
    draw_rows(&mut overlay, &theme, 80, 12);
    overlay.handle(&Event::Key(key(KeyCode::Home, KeyModifiers::NONE)), &keys);
    let first = draw_rows(&mut overlay, &theme, 80, 12).join("\n");
    assert!(first.contains("output-0 "), "{first}");
    assert!(!first.contains("output-99"), "{first}");
    overlay.handle(&Event::Key(key(KeyCode::End, KeyModifiers::NONE)), &keys);
    let last = draw_rows(&mut overlay, &theme, 80, 12).join("\n");
    assert!(last.contains("output-99"), "{last}");
    overlay.set_screen(vec!["new output".into()], None);
    let short = draw_rows(&mut overlay, &theme, 38, 12).join("\n");
    assert!(short.contains("new output"), "{short}");
}
