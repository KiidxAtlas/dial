use super::GitOverlay;
use crate::ui::keys::Keymap;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use std::path::Path;

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

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
