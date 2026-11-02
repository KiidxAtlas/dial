use super::*;
use crate::config::types::Config;

fn fixture() -> (tempfile::TempDir, GitOverlay) {
    let dir = tempfile::tempdir().unwrap();
    crate::git_panel::testkit::git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "first\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "second\n").unwrap();
    let mut overlay = GitOverlay::open(Repo::new(dir.path(), &Config::default()), None).unwrap();
    overlay.wait_reads();
    assert_eq!(overlay.changes.len(), 2, "{:?}", overlay.status);
    (dir, overlay)
}

#[test]
fn superseded_diff_never_replaces_the_selected_file() {
    let (_dir, mut overlay) = fixture();
    let (tx, rx) = mpsc::channel();
    overlay.reads.diff = Some(rx);
    overlay.cursor = 0;
    overlay.load_shown();
    let stale_generation = overlay.reads.generation;
    overlay.cursor = 1;
    overlay.load_shown();
    assert!(
        overlay.shown.is_none(),
        "old hunks are not actionable while loading"
    );
    tx.send((stale_generation, Err("stale failure".to_owned())))
        .unwrap();
    overlay.poll_reads();
    overlay.wait_reads();
    assert_eq!(overlay.shown.as_ref().unwrap().change.path, "b.txt");
    assert!(
        !overlay
            .status
            .as_ref()
            .is_some_and(|status| status.0.contains("stale failure"))
    );
}

#[test]
fn disconnected_worker_reports_failure_and_releases_its_slot() {
    let (_dir, mut overlay) = fixture();
    let (tx, rx) = mpsc::channel();
    overlay.reads.diff = Some(rx);
    drop(tx);
    overlay.poll_reads();
    assert!(overlay.reads.diff.is_none());
    assert!(
        overlay
            .status
            .as_ref()
            .is_some_and(|status| status.1 && status.0.contains("worker stopped"))
    );
}

#[test]
fn opening_does_not_wait_for_a_status_worker_and_can_be_closed() {
    use crate::ui::{keys::Keymap, overlay::OverlayEvent};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let (_dir, mut overlay) = fixture();
    let (_tx, rx) = mpsc::channel();
    overlay.reads.status = Some(rx);
    overlay.loading = true;
    // A blocked worker is represented by a live, empty channel: no wall-clock assertion.
    overlay.tick();
    assert!(matches!(
        overlay.handle(
            &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &Keymap::defaults()
        ),
        OverlayEvent::Close
    ));
}

#[test]
fn pending_or_failed_diff_cannot_stage_the_entire_file() {
    let (dir, mut overlay) = fixture();
    let (tx, rx) = mpsc::channel();
    overlay.reads.diff = Some(rx);
    overlay.load_shown();
    overlay.stage_current();
    let staged = || {
        std::process::Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    assert!(staged().stdout.is_empty());
    // Deliver a matching error through the real worker-result path, with no newer
    // queued read to replace it; the unavailable diff must still refuse staging.
    overlay.reads.pending_diff = None;
    tx.send((overlay.reads.generation, Err("diff failed".to_owned())))
        .unwrap();
    overlay.poll_reads();
    assert!(overlay.shown.is_none());
    assert!(overlay.status.as_ref().unwrap().0.contains("diff failed"));
    overlay.stage_current();
    assert!(staged().stdout.is_empty());
    assert!(
        overlay
            .status
            .as_ref()
            .unwrap()
            .0
            .contains("Diff unavailable")
    );
}

#[test]
fn reload_preserves_diff_cursor_until_matching_rows_arrive() {
    let (_dir, mut overlay) = fixture();
    assert!(overlay.rows.len() > 1);
    overlay.cursor_row = overlay.rows.len() - 1;
    let row = overlay.cursor_row;
    overlay.load_shown();
    assert!(overlay.rows.is_empty());
    assert_eq!(overlay.cursor_row, row);
    overlay.wait_reads();
    assert_eq!(overlay.cursor_row, row);
}

#[test]
fn status_refresh_invalidates_an_inflight_diff_before_it_can_arrive() {
    let (_dir, mut overlay) = fixture();
    let (tx, rx) = mpsc::channel();
    let generation = overlay.reads.generation;
    overlay.reads.diff = Some(rx);
    overlay.sync();
    let (_status_tx, status_rx) = mpsc::channel();
    overlay.reads.status = Some(status_rx);
    tx.send((generation, Err("stale old diff".to_owned())))
        .unwrap();
    overlay.poll_reads();
    assert!(overlay.shown.is_none());
    assert!(overlay.rows.is_empty());
    assert!(!overlay.mutation_ready());
    assert!(
        !overlay
            .status
            .as_ref()
            .unwrap()
            .0
            .contains("stale old diff")
    );
}
