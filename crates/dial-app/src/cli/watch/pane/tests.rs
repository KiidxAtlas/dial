use super::*;
use crate::agent::workflow::todo::Task;
use ratatui::{Terminal, backend::TestBackend};
use state::{Peers, Thread, Worker};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn state() -> Snapshot {
    Snapshot {
        version: state::VERSION,
        updated_ms: 1_000,
        busy: true,
        plan: vec![Task {
            content: "Wire the publisher".into(),
            status: Status::InProgress,
            phase: None,
            note: None,
            investigation: false,
            finding: Some("refresh_workers runs on both paths".into()),
            evidence: vec!["paint.rs:349".into()],
        }],
        jobs: vec![
            Job {
                kind: JobKind::Process,
                name: "server".into(),
                detail: "pnpm dev".into(),
                status: "running".into(),
                running: true,
                output: vec!["ready on :3000".into()],
            },
            Job {
                kind: JobKind::Command,
                name: "Update".into(),
                status: "finished".into(),
                ..Job::default()
            },
        ],
        workers: vec![Worker {
            id: "task-1".into(),
            name: "scout".into(),
            status: "running".into(),
            task: "map the owners".into(),
            cancellable: true,
            activity: vec!["tool: grep".into()],
            ..Worker::default()
        }],
        threads: vec![Thread {
            id: "1".into(),
            title: "why bg".into(),
            exchanges: Vec::new(),
            streaming: Some(state::Exchange {
                question: "why bg?".into(),
                answer: "because".into(),
                outcome: "streaming".into(),
            }),
        }],
        peers: Peers {
            rows: vec![state::PeerRow {
                id: "0123456789abcdef".into(),
                summary: "shortcut fixes".into(),
                phase: "working".into(),
                scopes: vec!["docs/terminal-interface.md".into()],
                overlapping: vec!["docs/terminal-interface.md".into()],
            }],
            ..Peers::default()
        },
        outcomes: vec!["Stopped from the side pane: killed server".into()],
    }
}

fn pane() -> Pane {
    let mut pane = Pane::new();
    pane.set_state(Ok(Some(state())));
    pane
}

fn screen(pane: &mut Pane, now_ms: u64) -> String {
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
    terminal
        .draw(|frame| pane.render(frame, &Theme::dark(), now_ms))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_tab_renders_its_section_with_counts() {
    let mut pane = pane();
    let feed = screen(&mut pane, 1_500);
    assert!(feed.contains("1 Feed"), "{feed}");
    assert!(feed.contains("Plan 0/1"), "{feed}");
    assert!(feed.contains("turn running"), "{feed}");
    assert!(feed.contains("Edits, reviews"), "{feed}");
    for (key_char, expected) in [
        ('2', "refresh_workers runs on both paths"),
        ('3', "ready on :3000"),
        ('4', "tool: grep"),
        ('5', "because"),
        ('6', "Overlaps yours"),
    ] {
        pane.key(key(KeyCode::Char(key_char)));
        let shown = screen(&mut pane, 1_500);
        assert!(shown.contains(expected), "tab {key_char}:\n{shown}");
    }
}

#[test]
fn tabs_cycle_with_arrows_and_wrap() {
    let mut pane = pane();
    pane.key(key(KeyCode::Left));
    assert_eq!(pane.tab, Tab::Peers);
    pane.key(key(KeyCode::Tab));
    assert_eq!(pane.tab, Tab::Feed);
}

#[test]
fn stopping_needs_confirmation_and_only_targets_running_work() {
    let mut pane = pane();
    pane.key(key(KeyCode::Char('3')));
    assert_eq!(pane.key(key(KeyCode::Char('x'))), KeyOutcome::Continue);
    assert!(screen(&mut pane, 1_500).contains("Stop server?"));
    assert_eq!(pane.key(key(KeyCode::Char('n'))), KeyOutcome::Continue);
    pane.key(key(KeyCode::Char('x')));
    assert_eq!(
        pane.key(key(KeyCode::Char('y'))),
        KeyOutcome::Send(Request::StopProcess {
            name: "server".into()
        })
    );
    // The finished command job below it cannot be stopped.
    pane.key(key(KeyCode::Down));
    pane.key(key(KeyCode::Char('x')));
    assert!(matches!(pane.mode, Mode::Browse));
    assert_eq!(
        pane.status.as_deref(),
        Some("Nothing running to stop here.")
    );

    pane.key(key(KeyCode::Char('4')));
    pane.key(key(KeyCode::Char('x')));
    assert_eq!(
        pane.key(key(KeyCode::Enter)),
        KeyOutcome::Send(Request::CancelWorker {
            id: "task-1".into()
        })
    );
    pane.key(key(KeyCode::Char('5')));
    pane.key(key(KeyCode::Char('x')));
    assert_eq!(
        pane.key(key(KeyCode::Char('y'))),
        KeyOutcome::Send(Request::CancelSideAnswer)
    );
}

#[test]
fn messaging_a_peer_composes_then_sends_and_escape_discards() {
    let mut pane = pane();
    pane.key(key(KeyCode::Char('6')));
    pane.key(key(KeyCode::Char('m')));
    for c in "hi".chars() {
        pane.key(key(KeyCode::Char(c)));
    }
    // `q` is text while composing, not quit.
    assert_eq!(pane.key(key(KeyCode::Char('q'))), KeyOutcome::Continue);
    pane.key(key(KeyCode::Backspace));
    pane.paste(" there\n");
    assert!(screen(&mut pane, 1_500).contains("To 01234567: hi there"));
    assert_eq!(
        pane.key(key(KeyCode::Enter)),
        KeyOutcome::Send(Request::MessagePeer {
            to: "0123456789abcdef".into(),
            text: "hi there".into()
        })
    );
    pane.key(key(KeyCode::Char('m')));
    pane.key(key(KeyCode::Char('x')));
    assert_eq!(pane.key(key(KeyCode::Esc)), KeyOutcome::Continue);
    assert!(matches!(pane.mode, Mode::Browse));
    assert_eq!(pane.key(key(KeyCode::Char('q'))), KeyOutcome::Quit);
}

#[test]
fn a_silent_session_and_missing_state_are_shown_plainly() {
    let mut pane = pane();
    assert!(screen(&mut pane, 60_000).contains("no update for 59s"));
    let mut empty = Pane::new();
    empty.set_state(Ok(None));
    empty.key(key(KeyCode::Char('2')));
    assert!(screen(&mut empty, 0).contains("Waiting for the session"));
    empty.set_state(Err(anyhow::anyhow!("version mismatch")));
    assert!(screen(&mut empty, 0).contains("version mismatch"));
}

#[test]
fn selection_is_clamped_when_rows_disappear() {
    let mut pane = pane();
    pane.key(key(KeyCode::Char('3')));
    pane.key(key(KeyCode::Down));
    assert_eq!(pane.selected(), 1);
    let mut fewer = state();
    fewer.jobs.truncate(1);
    pane.set_state(Ok(Some(fewer)));
    assert_eq!(pane.selected(), 0);
}

#[test]
fn feed_colours_diff_lines_and_scrolls_back() {
    let root = tempfile::tempdir().unwrap();
    let mut pane = pane();
    for n in 0..40 {
        pane.push_feed(
            root.path(),
            Event::Text {
                title: format!("edit {n}"),
                body: "+added\n-removed".into(),
            },
        );
    }
    let newest = screen(&mut pane, 1_500);
    assert!(newest.contains("edit 39") && !newest.contains("edit 0\n"));
    pane.key(key(KeyCode::Home));
    let oldest = screen(&mut pane, 1_500);
    assert!(oldest.contains("edit 0"), "{oldest}");
    pane.key(key(KeyCode::End));
    assert!(screen(&mut pane, 1_500).contains("edit 39"));
}
