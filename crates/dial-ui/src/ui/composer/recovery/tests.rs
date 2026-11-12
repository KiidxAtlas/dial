use super::*;
use crate::ui::composer::ComposerEvent;
use std::sync::Mutex;

#[test]
fn restart_preserves_collapsed_pastes_queue_intent_and_clipboard_after_original_deleted() {
    let mut first = Composer::new(Vec::new(), 100);
    first.queued.push_back(("auto".into(), false));
    first.queued.push_back(("later".into(), true));
    first.handle_paste(&"large pasted text\n".repeat(30));
    first
        .attach_image_bytes(b"clipboard-image-fixture")
        .unwrap();
    let expected = first.draft();
    let old_path = first.pasted_images[0].to_string_lossy().into_owned();
    let snapshot = first.recovery_snapshot().unwrap();
    drop(first);
    assert!(!std::path::Path::new(&old_path).exists());
    let mut restored = Composer::new(Vec::new(), 100);
    restored.restore_recovery(snapshot, false).unwrap();
    assert!(!restored.expanded_draft().contains(&old_path));
    assert!(
        restored
            .expanded_draft()
            .contains(&"large pasted text\n".repeat(30))
    );
    let new_path = restored.pasted_images[0].to_string_lossy();
    assert_eq!(
        restored.draft().replace(new_path.as_ref(), "IMAGE"),
        expected.replace(&old_path, "IMAGE")
    );
    assert_eq!(
        std::fs::read(&restored.pasted_images[0]).unwrap(),
        b"clipboard-image-fixture"
    );
    assert_eq!(
        restored.queued_messages(),
        vec![("auto".into(), false), ("later".into(), true)]
    );
    assert!(restored.take_follow_up().is_none());
    assert!(restored.take_steering().is_empty());
    assert!(!restored.has_pending_input());
    restored.resume_recovered_queue();
    assert_eq!(restored.take_steering(), vec!["auto"]);
    assert_eq!(restored.take_follow_up().as_deref(), Some("later"));
}

#[test]
fn received_and_applied_requests_never_reappear_but_pending_intent_remains_held() {
    let mut composer = Composer::new(Vec::new(), 100);
    for stage in [
        SteerStage::Pending,
        SteerStage::Received,
        SteerStage::Applied,
    ] {
        composer.explicit.push(super::super::Explicit {
            stage,
            untaken: stage == SteerStage::Pending,
            ..super::super::Explicit::new(format!("{stage:?}"), SteerKind::Independent)
        });
    }
    let snapshot = composer.recovery_snapshot().unwrap();
    let mut restored = Composer::new(Vec::new(), 100);
    restored.restore_recovery(snapshot, false).unwrap();
    restored.set_busy(false);
    assert!(restored.take_explicit_independent().is_empty());
    restored.resume_recovered_queue();
    assert_eq!(restored.take_explicit_independent(), vec!["Pending"]);
}

#[test]
fn overwrite_requires_explicit_choice_and_invalid_payload_is_transactional() {
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_draft("keep me");
    let saved = RecoverySnapshot {
        draft: "saved".into(),
        ..Default::default()
    };
    assert!(composer.restore_recovery(saved.clone(), false).is_err());
    assert_eq!(composer.draft(), "keep me");
    let invalid = RecoverySnapshot {
        images: vec![("old".into(), "not base64!".into())],
        ..saved.clone()
    };
    assert!(composer.restore_recovery(invalid, true).is_err());
    assert_eq!(composer.draft(), "keep me");
    composer.restore_recovery(saved, true).unwrap();
    assert_eq!(composer.draft(), "saved");
}

#[test]
fn execution_barrier_persists_removal_before_handoff_and_write_failures_keep_input() {
    let record = Arc::new(Mutex::new(RecoverySnapshot::default()));
    let output = record.clone();
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_recovery_sink(Some(Arc::new(move |snapshot| {
        *output.lock().unwrap() = snapshot.clone();
        Ok(())
    })));
    composer.set_draft("send once");
    assert!(matches!(composer.submit_voice(), ComposerEvent::Submit(text) if text == "send once"));
    assert!(record.lock().unwrap().is_empty());
    composer.set_draft("keep on failed save");
    composer.set_recovery_sink(Some(Arc::new(|_| anyhow::bail!("disk full"))));
    assert!(matches!(composer.submit_voice(), ComposerEvent::None));
    assert_eq!(composer.draft(), "keep on failed save");
    composer.queued.push_back(("queued".into(), false));
    assert!(composer.take_steering().is_empty());
    assert_eq!(composer.queued.len(), 1);
    assert!(
        composer
            .queue_notice
            .as_deref()
            .unwrap()
            .contains("disk full")
    );
}

#[test]
fn queue_edit_survives_restart_at_its_original_position() {
    let mut composer = Composer::new(Vec::new(), 100);
    composer
        .queued
        .extend([("first".into(), true), ("second".into(), false)]);
    assert!(composer.edit_queued(0));
    composer.editor.set("edited first");
    let snapshot = composer.recovery_snapshot().unwrap();
    let mut restored = Composer::new(Vec::new(), 100);
    restored.restore_recovery(snapshot, false).unwrap();
    restored.clear();
    assert_eq!(
        restored.queued_messages(),
        vec![("first".into(), true), ("second".into(), false)]
    );
}

#[test]
fn missing_clipboard_file_refuses_checkpoint_instead_of_saving_a_dead_attachment() {
    let mut composer = Composer::new(Vec::new(), 100);
    composer.attach_image_bytes(b"clipboard").unwrap();
    std::fs::remove_file(&composer.pasted_images[0]).unwrap();
    assert!(composer.recovery_snapshot().is_err());
}

#[test]
fn corrupt_counters_and_attachment_references_are_rejected_before_restoring() {
    let invalid = [
        RecoverySnapshot {
            pastes: vec![(String::new(), "payload".into())],
            ..Default::default()
        },
        RecoverySnapshot {
            images: vec![(String::new(), "aW1hZ2U=".into())],
            ..Default::default()
        },
        RecoverySnapshot {
            paste_count: usize::MAX,
            ..Default::default()
        },
    ];
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_draft("unchanged");
    for snapshot in invalid {
        assert!(composer.restore_recovery(snapshot, true).is_err());
        assert_eq!(composer.draft(), "unchanged");
    }
}

#[test]
fn preview_expands_pastes_labels_intent_and_omits_image_payloads_without_mutation() {
    let snapshot = RecoverySnapshot {
        draft: "before [paste] after".into(),
        pastes: vec![("[paste]".into(), "expanded paste".into())],
        queued: vec![("later".into(), true), ("automatic".into(), false)],
        explicit: vec![
            ("independent request".into(), true),
            ("steer request".into(), false),
        ],
        editing: Some((1, "editing text".into(), true)),
        images: vec![("clipboard.png".into(), "PRIVATE_IMAGE_PAYLOAD".into())],
        ..Default::default()
    };
    let before = snapshot.clone();
    let preview = snapshot.preview_text();
    for expected in [
        "before expanded paste after",
        "follow-up",
        "automatic steering",
        "independent request",
        "steer now",
        "editing text",
        "Clipboard images: 1",
    ] {
        assert!(preview.contains(expected), "missing {expected}");
    }
    assert!(!preview.contains("PRIVATE_IMAGE_PAYLOAD"));
    assert_eq!(snapshot, before);
    let large = RecoverySnapshot {
        draft: "界".repeat(5000),
        ..Default::default()
    };
    let preview = large.preview_text();
    assert!(preview.contains("preview abbreviated"));
    assert!(preview.chars().count() < 4200);
}
