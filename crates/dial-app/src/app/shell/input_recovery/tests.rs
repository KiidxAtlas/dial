use super::*;
use crate::ui::composer::{Composer, ComposerEvent};

fn draft(text: &str) -> RecoverySnapshot {
    let mut composer = Composer::new(Vec::new(), 10);
    composer.set_draft(text);
    composer.recovery_snapshot().unwrap()
}

#[test]
fn restart_is_session_scoped_and_a_second_writer_cannot_modify_or_discard() {
    let (_home, _project, app, _shell) = super::super::testkit::shell();
    let mut first = Store::open(&app.layout, "session-a").unwrap();
    first.checkpoint(&draft("private draft")).unwrap();
    assert!(Store::open(&app.layout, "session-a").is_err());
    let other = Store::open(&app.layout, "session-b").unwrap();
    assert!(other.record.pending.is_empty());
    drop(first);
    let reopened = Store::open(&app.layout, "session-a").unwrap();
    assert_eq!(reopened.record.pending, vec![draft("private draft")]);
    assert!(reopened.record.current.is_empty());
}

#[test]
fn corrupt_or_wrong_identity_records_are_not_overwritten() {
    let (_home, _project, app, _shell) = super::super::testkit::shell();
    let store = Store::open(&app.layout, "session-a").unwrap();
    let path = store.path.clone();
    drop(store);
    for bytes in [
        b"not JSON".to_vec(),
        serde_json::to_vec(&Record {
            version: 1,
            project: app.layout.project_root.join("wrong"),
            session: "session-a".into(),
            pending: vec![],
            current: draft("keep"),
        })
        .unwrap(),
    ] {
        atomic::write_private(&path, &bytes).unwrap();
        assert!(Store::open(&app.layout, "session-a").is_err());
        assert_eq!(atomic::read_private(&path).unwrap().unwrap(), bytes);
    }
}

#[test]
fn send_and_discard_survive_restart_without_resurrecting_input() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("send once");
    shell.checkpoint_input(true);
    assert!(
        matches!(shell.composer.submit_voice(), ComposerEvent::Submit(text) if text == "send once")
    );
    shell.recovery_session_changed("session-b");
    let reopened = Store::open(&app.layout, "session-a").unwrap();
    assert!(reopened.record.pending.is_empty());
    drop(reopened);
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("saved");
    shell.checkpoint_input(true);
    shell.recovery_session_changed("session-b");
    shell.poll_input_recovery(&app);
    assert!(shell.input_recovery_command("/draft discard"));
    shell.recovery_session_changed("session-b");
    let reopened = Store::open(&app.layout, "session-a").unwrap();
    assert!(reopened.record.pending.is_empty());
}

#[test]
fn restore_refuses_newer_draft_and_session_switch_does_not_transfer_input() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("origin");
    shell.set_session_id("session-b");
    shell.poll_input_recovery(&app);
    assert!(shell.composer.draft().is_empty());
    shell.composer.set_draft("destination");
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    assert!(shell.composer.draft().is_empty());
    shell.composer.set_draft("newer origin draft");
    assert!(shell.input_recovery_action("restore").is_err());
    assert_eq!(shell.composer.draft(), "newer origin draft");
    assert!(shell.input_recovery_action("replace").is_err());
    let preview = shell.preview_input_recovery().unwrap();
    shell
        .apply_input_review(preview, InputReviewChoice::ReplaceReviewed)
        .unwrap();
    assert_eq!(shell.composer.draft(), "origin");
    assert!(shell.composer.take_follow_up().is_none());
}

#[test]
fn delayed_timer_snapshot_cannot_resurrect_input_after_an_ownership_barrier() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("stale unsent input");
    let store = shell.input_recovery.store.as_ref().unwrap().clone();
    let mut writer = store.lock().unwrap();
    shell.input_recovery.checked = Some(Instant::now() - Duration::from_secs(1));
    shell.checkpoint_input(false);
    // Hold the writer until the newer execution/discard barrier has durably removed the input.
    shell.input_recovery.epoch.fetch_add(1, Ordering::AcqRel);
    writer.checkpoint(&RecoverySnapshot::default()).unwrap();
    drop(writer);
    shell
        .input_recovery
        .save_job
        .take()
        .unwrap()
        .recv()
        .unwrap()
        .unwrap();
    assert!(store.lock().unwrap().record.current.is_empty());
}

#[test]
fn explicit_corrupt_discard_respects_the_lease_and_keeps_current_input() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    let store = Store::open(&app.layout, "session-a").unwrap();
    let path = store.path.clone();
    atomic::write_private(&path, b"corrupt").unwrap();
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("current input");
    assert!(shell.input_recovery_action("discard").is_err());
    assert_eq!(atomic::read_private(&path).unwrap().unwrap(), b"corrupt");
    drop(store);
    shell.input_recovery_action("discard").unwrap();
    assert_eq!(shell.composer.draft(), "current input");
    let record: Record =
        serde_json::from_slice(&atomic::read_private(&path).unwrap().unwrap()).unwrap();
    assert!(record.pending.is_empty());
    assert_eq!(record.current, draft("current input"));
}

#[test]
fn failed_session_change_checkpoint_keeps_editor_queue_and_identity() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("must not disappear");
    let store = shell.input_recovery.store.as_ref().unwrap().clone();
    store.lock().unwrap().path = app.layout.state_dir.clone();
    assert!(shell.prepare_input_session_change().is_err());
    assert!(!shell.recovery_session_changed("session-b"));
    assert_eq!(shell.session_id, "session-a");
    assert_eq!(shell.composer.draft(), "must not disappear");
}

#[test]
fn canonical_size_clipboard_uses_authenticated_sidecar_and_aggregate_json_is_capped() {
    use base64::Engine;
    let (_home, _project, app, _shell) = super::super::testkit::shell();
    let mut store = Store::open(&app.layout, "session-a").unwrap();
    let image = vec![42; crate::llm::images::MAX_IMAGE_BYTES as usize];
    let snapshot: RecoverySnapshot = serde_json::from_value(serde_json::json!({
        "draft": "clipboard-path",
        "images": [["clipboard-path", base64::engine::general_purpose::STANDARD.encode(&image)]]
    }))
    .unwrap();
    store.checkpoint(&snapshot).unwrap();
    let path = store.path.clone();
    let original = atomic::read_private(&path).unwrap().unwrap();
    assert!(
        original.len() < 8192,
        "image bytes belong in the bounded private sidecar"
    );
    drop(store);
    let mut store = Store::open(&app.layout, "session-a").unwrap();
    assert_eq!(store.record.pending, vec![snapshot]);
    let mut next = store.record.clone();
    // The editor strips U+0001, so use accepted text whose quotes/backslashes survive editing
    // but double in JSON. Each snapshot remains below its 2 MiB text cap; the aggregate only
    // crosses the 8 MiB publication cap after JSON escaping.
    let escaped_text = "\"\\".repeat(450_000);
    let large = draft(&escaped_text);
    large.validate().unwrap();
    assert!(escaped_text.len() * 5 < 8 * 1024 * 1024);
    next.pending = vec![large.clone(); 4];
    next.current = large;
    assert!(
        serde_json::to_vec(&next).unwrap().len() > 8 * 1024 * 1024,
        "the serialized fixture must actually cross the aggregate 8 MiB boundary"
    );
    assert!(
        store
            .publish(next)
            .unwrap_err()
            .to_string()
            .contains("8 MiB")
    );
    assert_eq!(atomic::read_private(&path).unwrap().unwrap(), original);
}

fn saved_shell() -> (tempfile::TempDir, tempfile::TempDir, App, Shell) {
    let (home, project, app, mut shell) = super::super::testkit::shell();
    let mut store = Store::open(&app.layout, "session-a").unwrap();
    let snapshot = serde_json::from_value(serde_json::json!({
        "draft": "saved draft",
        "queued": [["automatic", false], ["follow-up", true]],
        "explicit": [["independent", true], ["steer now", false]]
    }))
    .unwrap();
    store.checkpoint(&snapshot).unwrap();
    drop(store);
    shell.set_session_id("session-a");
    shell.poll_input_recovery(&app);
    (home, project, app, shell)
}

#[test]
fn preview_and_keep_are_read_only_and_restore_holds_every_queue_intent() {
    let (_home, _project, _app, mut shell) = saved_shell();
    let store = shell.input_recovery.store.as_ref().unwrap().clone();
    let path = store.lock().unwrap().path.clone();
    let bytes = atomic::read_private(&path).unwrap();
    let preview = shell.preview_input_recovery().unwrap();
    let text = preview.text();
    for expected in [
        "saved draft",
        "automatic",
        "follow-up",
        "independent",
        "steer now",
    ] {
        assert!(text.contains(expected), "missing {expected} in preview");
    }
    shell
        .apply_input_review(preview, InputReviewChoice::Keep)
        .unwrap();
    assert!(shell.composer.draft().is_empty());
    assert_eq!(atomic::read_private(&path).unwrap(), bytes);
    let preview = shell.preview_input_recovery().unwrap();
    shell
        .apply_input_review(preview, InputReviewChoice::RestoreHeld)
        .unwrap();
    assert_eq!(shell.composer.draft(), "saved draft");
    assert!(shell.composer.take_follow_up().is_none());
    assert!(shell.composer.take_steering().is_empty());
    assert!(shell.composer.take_explicit_independent().is_empty());
    assert!(shell.composer.take_explicit_steering().is_empty());
    assert!(store.lock().unwrap().record.pending.is_empty());
}

#[test]
fn review_refuses_conflicts_and_stale_current_saved_or_session_ownership() {
    let (_home, _project, app, mut shell) = saved_shell();
    shell.composer.set_draft("newer draft");
    let preview = shell.preview_input_recovery().unwrap();
    assert!(
        shell
            .apply_input_review(preview, InputReviewChoice::RestoreHeld)
            .is_err()
    );
    assert_eq!(shell.composer.draft(), "newer draft");
    for choice in [
        InputReviewChoice::ReplaceReviewed,
        InputReviewChoice::Discard,
    ] {
        let preview = shell.preview_input_recovery().unwrap();
        shell.composer.set_draft("changed during review");
        assert!(shell.apply_input_review(preview, choice).is_err());
        assert_eq!(shell.composer.draft(), "changed during review");
        shell.composer.set_draft("newer draft");
    }
    let preview = shell.preview_input_recovery().unwrap();
    let store = shell.input_recovery.store.as_ref().unwrap().clone();
    store
        .lock()
        .unwrap()
        .record
        .pending
        .push(draft("another saved record"));
    assert!(
        shell
            .apply_input_review(preview, InputReviewChoice::ReplaceReviewed)
            .is_err()
    );
    assert_eq!(shell.composer.draft(), "newer draft");
    let preview = shell.preview_input_recovery().unwrap();
    shell.set_session_id("session-b");
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("destination input");
    assert!(
        shell
            .apply_input_review(preview, InputReviewChoice::ReplaceReviewed)
            .is_err()
    );
    assert_eq!(shell.composer.draft(), "destination input");
}

#[test]
fn reviewed_discard_keeps_current_and_consumed_preview_cannot_replace_again() {
    let (_home, _project, _app, mut shell) = saved_shell();
    shell.composer.set_draft("keep current");
    let stale = shell.preview_input_recovery().unwrap();
    let preview = shell.preview_input_recovery().unwrap();
    shell
        .apply_input_review(preview, InputReviewChoice::Discard)
        .unwrap();
    assert_eq!(shell.composer.draft(), "keep current");
    assert!(
        shell
            .apply_input_review(stale, InputReviewChoice::ReplaceReviewed)
            .is_err()
    );
    assert_eq!(shell.composer.draft(), "keep current");
    let record = &shell
        .input_recovery
        .store
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .record;
    assert!(record.pending.is_empty());
    assert_eq!(record.current, draft("keep current"));
}
