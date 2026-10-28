use super::*;

#[test]
fn merge_keeps_fields_absent_from_the_update() {
    let mut settings = WorkerSettings {
        model: Some("a".into()),
        thinking: Some("high".into()),
    };
    settings.merge(WorkerSettings {
        model: None,
        thinking: Some("low".into()),
    });
    assert_eq!(settings.model.as_deref(), Some("a"));
    assert_eq!(settings.thinking.as_deref(), Some("low"));
}

#[test]
fn empty_update_is_detected() {
    assert!(WorkerSettings::default().is_empty());
    assert!(
        !WorkerSettings {
            model: Some("a".into()),
            thinking: None,
        }
        .is_empty()
    );
}

#[test]
fn status_tracks_queued_applying_applied_and_failed_revisions() {
    let mut slot = SettingsSlot::default();
    assert_eq!(
        slot.status(),
        "config rev 0 launch config; model inherited, thinking inherited"
    );

    slot.accepted.thinking = Some("low".into());
    slot.revision = 1;
    slot.queued = true;
    assert!(slot.status().starts_with("config rev 1 queued;"));

    slot.queued = false;
    slot.inflight = Some(1);
    assert!(slot.status().starts_with("config rev 1 applying;"));

    slot.inflight = None;
    slot.applied_revision = 1;
    assert!(slot.status().starts_with("config rev 1 applied;"));

    slot.revision = 2;
    slot.queued = true;
    slot.failure = Some((1, "unknown model".into()));
    assert!(
        slot.status().starts_with("config rev 2 queued;"),
        "a newer queued update is never masked by an older failure"
    );

    slot.queued = false;
    slot.failure = Some((2, "unknown model".into()));
    assert_eq!(
        slot.status(),
        "config rev 2 not applied: unknown model; model inherited, thinking low"
    );
}

#[test]
fn late_acknowledgement_never_marks_a_newer_queued_revision_applied() {
    let mut slot = SettingsSlot::default();
    slot.accepted.model = Some("a".into());
    slot.revision = 1;
    slot.queued = true;

    // Request boundary takes revision 1; configure arrives during backend construction.
    assert!(slot.take().is_some());
    slot.accepted.thinking = Some("low".into());
    slot.revision = 2;
    slot.queued = true;

    // The late acknowledgement applies only the captured revision.
    slot.acknowledge();
    assert_eq!(slot.applied_revision, 1);
    assert!(slot.queued, "revision 2 stays queued");
    assert!(slot.status().starts_with("config rev 2 queued;"));

    // The next boundary takes revision 2 and its failure is scoped to revision 2 only.
    assert!(slot.take().is_some());
    slot.fail("unknown model");
    assert_eq!(slot.applied_revision, 1);
    assert!(
        slot.status()
            .starts_with("config rev 2 not applied: unknown model;")
    );
}

#[test]
fn failed_take_keeps_previous_backend_and_leaves_later_queue_intact() {
    let mut slot = SettingsSlot::default();
    slot.revision = 1;
    slot.queued = true;
    assert!(slot.take().is_some());
    slot.fail("journal write failed");
    assert_eq!(slot.applied_revision, 0);
    assert!(
        !slot.applied,
        "a failed application must not retire the old backend"
    );
    assert!(slot.take().is_none(), "nothing is queued after the failure");
}
