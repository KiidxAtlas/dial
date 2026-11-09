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
            kind: crate::agent::workflow::todo::Kind::Step,
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
                model: "claude-opus".into(),
                messages: vec![
                    state::PeerMessage {
                        outgoing: true,
                        text: "I am editing the keymap".into(),
                        unread: false,
                        at_ms: 1_000,
                        by_user: true,
                    },
                    state::PeerMessage {
                        outgoing: false,
                        text: "ok, I will stay in docs".into(),
                        unread: true,
                        at_ms: 1_200,
                        by_user: false,
                    },
                ],
            }],
            ..Peers::default()
        },
        outcomes: vec!["Stopped from the side pane: killed server".into()],
        model: "gpt-5".into(),
        conversation: vec![
            state::ChatLine {
                role: ChatRole::User,
                text: "fix the parser".into(),
                at_ms: 500,
            },
            state::ChatLine {
                role: ChatRole::Event,
                text: "Woken by a project peer message".into(),
                at_ms: 900,
            },
            state::ChatLine {
                role: ChatRole::Model,
                text: "Parser fixed; tests pass.".into(),
                at_ms: 1_000,
            },
        ],
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
        ('6', "Enter opens the conversation"),
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

/// Open the Chats tab's conversation on row `row` (0 is this session).
fn open(pane: &mut Pane, row: usize) {
    pane.key(key(KeyCode::Char('6')));
    for _ in 0..row {
        pane.key(key(KeyCode::Down));
    }
    pane.key(key(KeyCode::Enter));
}

#[test]
fn the_chats_list_shows_this_session_then_peers_with_previews_and_unread_counts() {
    let mut pane = pane();
    pane.key(key(KeyCode::Char('6')));
    let list = screen(&mut pane, 1_500);
    assert_eq!(pane.badge(Tab::Peers), " 1 new");
    for expected in [
        "gpt-5 (this session)",
        "Parser fixed; tests pass.",
        "claude-opus · 01234567 · working",
        "1 new",
        "overlaps",
    ] {
        assert!(list.contains(expected), "{expected}:\n{list}");
    }
    pane.key(key(KeyCode::Down));
    assert!(screen(&mut pane, 1_500).contains("Overlaps yours"));
}

#[test]
fn a_peer_conversation_reads_like_a_text_thread_with_receipts() {
    let mut pane = pane();
    open(&mut pane, 1);
    let chat = screen(&mut pane, 61_000);
    for expected in [
        "● claude-opus · 01234567 · working · shortcut fixes",
        "You · 1m",
        "I am editing the keymap",
        "Read",
        "claude-opus · 1m",
        "ok, I will stay in docs",
        "Not yet read by gpt-5",
        "claude-opus is working…",
        "› Message claude-opus…",
    ] {
        assert!(chat.contains(expected), "{expected}:\n{chat}");
    }
    // Outgoing bubbles hug the right edge, incoming ones the left.
    let row = |needle: &str| {
        chat.lines()
            .find(|line| line.contains(needle))
            .unwrap()
            .to_owned()
    };
    assert!(row("I am editing the keymap").trim_end().len() > 60);
    assert!(row("ok, I will stay in docs").starts_with(" ok"));
}

#[test]
fn typing_in_a_conversation_sends_to_the_peer_and_shows_it_until_published() {
    let mut pane = pane();
    open(&mut pane, 1);
    for c in "hi".chars() {
        pane.key(key(KeyCode::Char(c)));
    }
    // `q` is text in a conversation, not quit.
    assert_eq!(pane.key(key(KeyCode::Char('q'))), KeyOutcome::Continue);
    pane.key(key(KeyCode::Backspace));
    pane.paste(" there\n");
    assert!(screen(&mut pane, 1_500).contains("› hi there"));
    assert_eq!(
        pane.key(key(KeyCode::Enter)),
        KeyOutcome::Send(Request::MessagePeer {
            to: "0123456789abcdef".into(),
            text: "hi there".into()
        })
    );
    let sent = screen(&mut pane, crate::ui::subagent_model::epoch_ms());
    assert!(
        sent.contains("hi there") && sent.contains("Sending…"),
        "{sent}"
    );
    assert!(pane.draft.is_empty());
    // Once the session publishes the message, the placeholder gives way to it.
    let mut published = state();
    published.peers.rows[0].messages.push(state::PeerMessage {
        outgoing: true,
        text: "hi there".into(),
        unread: true,
        at_ms: crate::ui::subagent_model::epoch_ms(),
        by_user: true,
    });
    pane.set_state(Ok(Some(published)));
    assert!(pane.outbox.is_empty());
    assert!(screen(&mut pane, crate::ui::subagent_model::epoch_ms()).contains("Delivered"));
    // Enter on an empty draft sends nothing; Esc clears a draft, then leaves the conversation.
    assert_eq!(pane.key(key(KeyCode::Enter)), KeyOutcome::Continue);
    pane.key(key(KeyCode::Char('x')));
    pane.key(key(KeyCode::Esc));
    assert!(pane.draft.is_empty() && pane.chat.is_some());
    pane.key(key(KeyCode::Esc));
    assert!(pane.chat.is_none());
    assert_eq!(pane.key(key(KeyCode::Char('q'))), KeyOutcome::Quit);
}

#[test]
fn this_sessions_thread_prompts_its_own_model() {
    let mut pane = pane();
    open(&mut pane, 0);
    let chat = screen(&mut pane, 1_500);
    for expected in [
        "● gpt-5 · this session",
        "fix the parser",
        "Woken by a project peer message",
        "Parser fixed; tests pass.",
        "gpt-5 is working…",
    ] {
        assert!(chat.contains(expected), "{expected}:\n{chat}");
    }
    for c in "run the tests".chars() {
        pane.key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        pane.key(key(KeyCode::Enter)),
        KeyOutcome::Send(Request::Prompt {
            text: "run the tests".into()
        })
    );
    // Switching tabs closes the conversation.
    pane.key(key(KeyCode::Tab));
    assert!(pane.chat.is_none() && pane.tab == Tab::Feed);
}

#[test]
fn a_peer_that_leaves_is_reported_and_cannot_be_messaged() {
    let mut pane = pane();
    open(&mut pane, 1);
    let mut gone = state();
    gone.peers.rows.clear();
    pane.set_state(Ok(Some(gone)));
    assert!(screen(&mut pane, 1_500).contains("Peer 01234567 is no longer active"));
    pane.key(key(KeyCode::Char('x')));
    assert_eq!(pane.key(key(KeyCode::Enter)), KeyOutcome::Continue);
    assert!(pane.status.as_deref().unwrap().contains("nothing was sent"));
}

#[test]
fn ages_are_compact_and_unknown_times_stay_blank() {
    assert_eq!(chat::age(10_000, 0), "");
    assert_eq!(chat::age(10_000, 9_000), "now");
    assert_eq!(chat::age(120_000, 1), "1m");
    assert_eq!(chat::age(7_200_000 + 5, 5), "2h");
    assert_eq!(chat::age(3 * 86_400_000 + 5, 5), "3d");
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
