use super::GitOverlay;
use crate::git_panel::testkit::git;
use crate::ui::keys::Keymap;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

fn delete(kind: KeyEventKind) -> Event {
    Event::Key(KeyEvent {
        code: KeyCode::Delete,
        modifiers: KeyModifiers::NONE,
        kind,
        state: KeyEventState::NONE,
    })
}

#[test]
fn a_held_delete_key_never_confirms_its_own_discard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("f.txt"), "one\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
    std::fs::write(root.join("f.txt"), "changed\n").expect("write");

    let keys = Keymap::defaults();
    let mut overlay = GitOverlay::open(
        crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
        None,
    )
    .expect("overlay opens");
    overlay.wait_reads();
    assert!(
        overlay.selected().is_some(),
        "the modified file is selected"
    );

    overlay.handle(&delete(KeyEventKind::Press), &keys);
    for _ in 0..5 {
        overlay.handle(&delete(KeyEventKind::Repeat), &keys);
    }
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "changed\n",
        "auto-repeat must not confirm the discard"
    );
    assert!(overlay.confirm.is_some(), "the confirmation stays armed");

    overlay.handle(&delete(KeyEventKind::Release), &keys);
    overlay.handle(&delete(KeyEventKind::Press), &keys);
    overlay.wait_mutation();
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "one\n",
        "a second deliberate press discards"
    );
}

#[test]
fn git_tabs_route_focus_preserve_drafts_and_keep_revisions_read_only() {
    use super::{GitTab, model::Focus};
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    let keys = Keymap::defaults();
    for revision in [None, Some("HEAD")] {
        let mut overlay = GitOverlay::open(
            crate::git_panel::Repo::new(dir.path(), &crate::config::types::Config::default()),
            revision,
        )
        .expect("overlay opens asynchronously");
        overlay.message.set("draft stays here");
        for (name, tab, focus, editing) in [
            ("status", GitTab::Status, Focus::Sidebar, false),
            ("diff", GitTab::Diff, Focus::Diff, false),
            ("commit", GitTab::Commit, Focus::Sidebar, revision.is_none()),
            ("log", GitTab::Log, Focus::Sidebar, false),
        ] {
            assert_eq!(GitTab::parse(name), Some(tab));
            overlay.select_tab(tab);
            assert_eq!(overlay.focus, focus);
            assert_eq!(overlay.editing, editing);
            assert_eq!(overlay.message.text, "draft stays here");
        }
        overlay.select_tab(GitTab::Status);
        for expected in if revision.is_none() {
            vec![GitTab::Diff, GitTab::Commit, GitTab::Log, GitTab::Status]
        } else {
            vec![GitTab::Diff, GitTab::Status]
        } {
            overlay.key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &keys);
            assert_eq!(overlay.tab(), expected);
        }
        overlay.key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &keys);
        assert_eq!(
            overlay.tab(),
            if revision.is_none() {
                GitTab::Log
            } else {
                GitTab::Diff
            }
        );
        assert_eq!(overlay.message.text, "draft stays here");
    }
    assert_eq!(GitTab::parse("HEAD~2"), None);
}

#[test]
fn git_named_tab_refs_keep_their_original_single_argument_meaning() {
    use super::GitTab;
    // `/git status` (and similarly named refs) must still be passed to revision loading.
    for name in ["status", "diff", "commit", "log", "HEAD", "main"] {
        assert_eq!(GitTab::from_args(name), None, "/git {name} is a revision");
    }
    for (args, expected) in [
        ("tab status", GitTab::Status),
        ("tab diff", GitTab::Diff),
        ("tab commit", GitTab::Commit),
        ("tab log", GitTab::Log),
    ] {
        assert_eq!(GitTab::from_args(args), Some(expected));
    }
}

#[test]
fn git_log_uses_the_status_snapshot_and_opens_a_selected_revision() {
    use super::GitTab;
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q"]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "history subject",
        ],
    );
    let mut panel = GitOverlay::open(
        crate::git_panel::Repo::new(dir.path(), &crate::config::types::Config::default()),
        None,
    )
    .expect("panel");
    panel.wait_reads();
    let entries = panel.recent.as_ref().expect("recent commits");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].1.ends_with("history subject"));
    let expected = format!("command:/git {}", entries[0].0);
    panel.select_tab(GitTab::Log);
    assert!(
        matches!(panel.handle(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), &Keymap::defaults()), crate::ui::overlay::OverlayEvent::Action(action) if action == expected)
    );
}

impl GitOverlay {
    pub(super) fn wait_mutation(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while self.mutation.is_some() {
            assert!(std::time::Instant::now() < deadline, "mutation timed out");
            self.poll_jobs();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.wait_reads();
    }
}

fn modified_panel() -> (tempfile::TempDir, GitOverlay) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::write(root.join("f"), "one\ntwo\nthree\n").unwrap();
    git(root, &["add", "f"]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "init"],
    );
    std::fs::write(root.join("f"), "one\nTWO\nthree\n").unwrap();
    let mut panel = GitOverlay::open(
        crate::git_panel::Repo::new(root, &crate::config::types::Config::default()),
        None,
    )
    .unwrap();
    panel.wait_reads();
    panel.focus = super::model::Focus::Diff;
    panel.cursor_row = panel
        .rows
        .iter()
        .position(|row| {
            matches!(
                &row.kind,
                super::model::RowKind::Line {
                    side: super::model::Side::Added,
                    ..
                }
            )
        })
        .unwrap();
    (dir, panel)
}

#[test]
fn destructive_x_requires_fresh_deliberate_confirmation_for_hunks_and_lines() {
    for marked in [false, true] {
        let (dir, mut panel) = modified_panel();
        let keys = Keymap::defaults();
        let press = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        if marked {
            panel.handle(&press('m'), &keys);
        }
        panel.handle(&press('x'), &keys);
        assert!(panel.mutation.is_none());
        let mut repeat = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        panel.handle(&Event::Key(repeat), &keys);
        assert!(panel.mutation.is_none());
        // A refreshed snapshot must never inherit an earlier destructive confirmation.
        panel.sync();
        panel.wait_reads();
        panel.cursor_row = panel
            .rows
            .iter()
            .position(|row| {
                matches!(
                    &row.kind,
                    super::model::RowKind::Line {
                        side: super::model::Side::Added,
                        ..
                    }
                )
            })
            .unwrap();
        if marked {
            panel.handle(&press('m'), &keys);
        }
        panel.handle(&press('x'), &keys);
        assert!(panel.mutation.is_none());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f")).unwrap(),
            "one\nTWO\nthree\n"
        );
        panel.handle(&press('x'), &keys);
        assert!(panel.mutation.is_some());
        panel.wait_mutation();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f")).unwrap(),
            if marked {
                "one\nthree\n"
            } else {
                "one\ntwo\nthree\n"
            }
        );
    }
}

#[test]
fn commit_and_ai_fields_edit_and_paste_at_the_cursor() {
    let (_dir, mut panel) = modified_panel();
    let keys = Keymap::defaults();
    for ai in [false, true] {
        panel.editing = !ai;
        if ai {
            panel.open_ai_prompt();
        }
        panel.handle(&Event::Paste("ab".into()), &keys);
        panel.handle(
            &Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            &keys,
        );
        panel.handle(&Event::Paste("中".into()), &keys);
        let editor = if ai {
            panel.ai_prompt.as_ref().unwrap()
        } else {
            &panel.message
        };
        assert_eq!(editor.text, "a中b");
        assert_eq!(editor.cursor, "a中".len());
    }
}

#[test]
fn mutation_is_nonblocking_refuses_overlap_and_keeps_newer_draft() {
    let (_dir, mut panel) = modified_panel();
    panel.message.set("submitted");
    panel.commit_draft = Some("submitted".into());
    let (release, wait) = std::sync::mpsc::channel();
    panel.start_mutation(move |_| {
        wait.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        Ok(("Committed".into(), true))
    });
    assert!(panel.mutation.is_some());
    assert!(
        !panel.mutation_ready(),
        "overlapping Git mutation is refused"
    );
    panel.message.set("new draft while committing");
    release.send(()).unwrap();
    panel.wait_mutation();
    assert_eq!(panel.message.text, "new draft while committing");
}

#[test]
fn failed_commit_preserves_draft_and_amend_state() {
    let (_dir, mut panel) = modified_panel();
    panel.message.set("keep this draft");
    panel.editing = true;
    panel.commit(); // No staged changes, so Git must refuse.
    panel.wait_mutation();
    assert_eq!(panel.message.text, "keep this draft");
    assert!(panel.editing);
    assert!(panel.status.as_ref().unwrap().1);
}

#[test]
fn rebound_navigation_wins_over_panel_letters_and_disabled_keys_stay_disabled() {
    use crate::ui::keys::Action;
    let (_dir, mut panel) = modified_panel();
    let mut keys = Keymap::defaults();
    keys.rebind(Action::SelectDown, &["r"]).unwrap();
    keys.rebind(Action::SelectUp, &[]).unwrap();
    let before = panel.cursor_row;
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        &keys,
    );
    assert_eq!(panel.cursor_row, before + 1);
    assert!(
        !panel.reads.busy(),
        "rebound r navigates instead of refreshing"
    );
    panel.handle(
        &Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &keys,
    );
    assert_eq!(
        panel.cursor_row,
        before + 1,
        "disabled Up must not use a raw fallback"
    );
}

#[test]
fn reopening_a_repository_cannot_overlap_an_inflight_git_operation() {
    let (dir, mut first) = modified_panel();
    let (release, wait) = std::sync::mpsc::channel();
    first.start_mutation(move |_| {
        wait.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        Err("held operation ended".into())
    });
    // Closing the receiver must not release admission while its worker is still running.
    drop(first);
    let mut reopened = GitOverlay::open(
        crate::git_panel::Repo::new(dir.path(), &crate::config::types::Config::default()),
        None,
    )
    .unwrap();
    reopened.wait_reads();
    reopened.stage_all();
    reopened.wait_mutation();
    let staged = git(dir.path(), &["diff", "--cached", "--name-only"]);
    let refused = reopened.status.as_ref().is_some_and(|(_, error, _)| *error);
    release.send(()).unwrap();
    assert!(
        refused,
        "same-repository overlapping operation must be refused"
    );
    assert!(
        staged.is_empty(),
        "a reopened panel must not stage during the old operation"
    );
}

fn panel_screen(panel: &mut GitOverlay) -> String {
    use crate::ui::overlay::{
        DrawCtx,
        settings::testkit::{Rig, render, screen, status_preview},
    };
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    screen(&render(120, 24, |frame| panel.draw(frame, &ctx)))
}

#[test]
fn reopened_panel_reports_the_failure_of_a_closed_panels_write() {
    for complete_before_reopen in [false, true] {
        let (dir, mut first) = modified_panel();
        let repo = first.repo.clone();
        let (release, wait) = std::sync::mpsc::channel();
        first.start_mutation(move |_| {
            wait.recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            Err("pre-commit hook refused retained-write-test".into())
        });
        drop(first);
        if complete_before_reopen {
            release.send(()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while repo.job_notice().is_some_and(|notice| notice.running) {
                assert!(std::time::Instant::now() < deadline, "worker completes");
                std::thread::yield_now();
            }
        }
        let mut reopened = GitOverlay::open(
            crate::git_panel::Repo::new(dir.path(), &crate::config::types::Config::default()),
            None,
        )
        .unwrap();
        reopened.wait_reads();
        if !complete_before_reopen {
            assert!(panel_screen(&mut reopened).contains("Git write in progress"));
            release.send(()).unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            reopened.tick();
            if reopened.status.as_ref().is_some_and(|(message, error, _)| {
                *error && message.contains("pre-commit hook refused retained-write-test")
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "closed-panel write failure must remain visible on reopen"
            );
            std::thread::yield_now();
        }
        reopened.wait_reads();
        // Recovery is not an expiring toast, and both main surfaces show it.
        if let Some((_, _, at)) = reopened.status.as_mut() {
            *at = std::time::Instant::now() - super::model::STATUS_TTL;
        }
        for tab in [super::GitTab::Status, super::GitTab::Log] {
            reopened.select_tab(tab);
            assert!(
                panel_screen(&mut reopened).contains("pre-commit hook refused retained-write-test")
            );
        }
        reopened.handle(
            &Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            &Keymap::defaults(),
        );
        assert!(
            !reopened.show_repo_outcome,
            "input acknowledges the recovered result without retrying"
        );
        assert!(git(dir.path(), &["diff", "--cached", "--name-only"]).is_empty());
    }
}
