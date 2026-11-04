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
        overlay.message = "draft stays here".into();
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
            assert_eq!(overlay.message, "draft stays here");
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
        assert_eq!(overlay.message, "draft stays here");
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
