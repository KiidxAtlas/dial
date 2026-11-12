use super::*;

#[test]
fn deduplicates_without_reopening_read_notices_and_bounds_session_memory() {
    let mut inbox = Inbox::default();
    inbox.push("Workers", "finished");
    inbox.push("Workers", "finished");
    assert_eq!(inbox.unread(), 1);
    assert_eq!(inbox.read(1), Some(("Workers", "finished")));
    inbox.push("Workers", "finished");
    // Adding typed metadata later does not reopen an existing read notice.
    inbox.push_category("Workers", "finished", Category::Completion);
    assert_eq!(inbox.unread(), 0);
    assert!(inbox.read(999).is_none());
    for i in 0..LIMIT {
        inbox.push("Background", &format!("job {i}"));
    }
    // Notices, mark-all-read and the general settings row.
    assert_eq!(inbox.choices().items.len(), LIMIT + 2);
    assert!(inbox.read(1).is_none());
    assert_eq!(inbox.choices().items[0].value, format!("{}", LIMIT + 1));
}

#[test]
fn preferences_action_never_reads_a_notice() {
    let mut inbox = Inbox::default();
    inbox.push("Worker", "needs input");
    let spec = inbox.choices();
    assert_eq!(spec.command, "inbox");
    let preferences = spec
        .items
        .iter()
        .find(|item| item.value == "notifications")
        .unwrap();
    assert_eq!(preferences.label, "Notification settings");
    assert_eq!(inbox.unread(), 1);
    assert_eq!(inbox.read(1), Some(("Worker", "needs input")));
    assert_eq!(inbox.unread(), 0);
    assert!(
        Inbox::default()
            .choices()
            .items
            .iter()
            .any(|item| item.value == "notifications")
    );
}

#[test]
fn individual_preferences_use_explicit_categories_without_reading_or_dismissing() {
    for (category, expected, label) in [
        (Category::General, "ui.notify", "Notification settings"),
        (
            Category::Completion,
            "ui.notify",
            "Completion notification settings",
        ),
        (
            Category::Error,
            "ui.notify_errors",
            "Error notification settings",
        ),
    ] {
        let mut inbox = Inbox::default();
        inbox.push_category("Same title", "Same body", category);
        let top = inbox.choices();
        assert_eq!(top.items[0].value, "1", "Enter reads the notice directly");
        let menu = inbox.notice_choices(1).unwrap();
        assert_eq!(menu.command, "inbox");
        assert_eq!(
            menu.items[0].value, "1",
            "legacy numeric read route retained"
        );
        assert_eq!(menu.items[1].value, "notifications 1");
        assert_eq!(menu.items[1].label, label);
        assert_eq!(inbox.settings_path(1), Some(expected));
        assert!(
            crate::config::schema::registry()
                .iter()
                .any(|def| def.path == expected)
        );
        assert!(menu.remove.is_none(), "preferences are not dismissal");
        assert_eq!(inbox.unread(), 1);
        assert_eq!(inbox.entries.len(), 1);
        assert_eq!(inbox.read(1), Some(("Same title", "Same body")));
        inbox.notice_choices(1).unwrap();
        assert_eq!(inbox.settings_path(1), Some(expected));
        assert_eq!(
            inbox.unread(),
            0,
            "opening preferences does not reopen a read notice"
        );
    }
}

#[test]
fn legacy_notices_never_infer_categories_from_arbitrary_text() {
    let mut inbox = Inbox::default();
    for title in ["Error", "Worker finished", "Sound", "Unknown"] {
        inbox.push(title, "Turn failed: worker completion sound notification");
    }
    for id in 1..=4 {
        assert_eq!(inbox.settings_path(id), Some("ui.notify"));
        let menu = inbox.notice_choices(id).unwrap();
        assert_eq!(menu.items[1].label, "Notification settings");
        assert!(
            menu.items[1]
                .detail
                .contains("No category-specific preference")
        );
    }
    assert!(inbox.notice_choices(999).is_none());
    assert_eq!(inbox.settings_path(999), None);
    assert_eq!(inbox.unread(), 4);
}

#[test]
fn enter_reads_directly_unread_is_a_mark_and_mark_all_read_clears_every_notice() {
    let mut inbox = Inbox::default();
    inbox.push("Worker", "done");
    inbox.push_category("Build", "failed", Category::Error);
    let spec = inbox.choices();
    assert_eq!(spec.verb, "open");
    assert_eq!(spec.current_tag.as_deref(), Some("unread"));
    assert!(
        spec.items
            .iter()
            .all(|item| !item.label.starts_with("Unread"))
    );
    let unread: Vec<_> = spec
        .items
        .iter()
        .filter(|i| i.current)
        .map(|i| &i.value)
        .collect();
    assert_eq!(unread, ["2", "1"]);
    let values: Vec<_> = spec.items.iter().map(|i| i.value.as_str()).collect();
    assert_eq!(
        values,
        ["2", "1", "read-all", "notifications 2", "notifications"],
        "per-category settings appear only for categories present"
    );
    assert_eq!(inbox.read_all(), 2);
    assert_eq!(inbox.unread(), 0);
    assert_eq!(inbox.read_all(), 0);
    assert!(
        inbox
            .choices()
            .items
            .iter()
            .all(|i| i.value != "read-all" && !i.current),
        "nothing left to mark"
    );
}

#[test]
fn return_digest_lists_only_notices_that_arrived_during_a_real_focus_loss() {
    let mut inbox = Inbox::default();
    inbox.push("Workers", "before any focus report");
    // Unknown focus reads as unfocused; that is not an absence.
    assert_eq!(inbox.returned(false), None);
    assert_eq!(inbox.returned(true), None);

    assert_eq!(inbox.returned(false), None);
    inbox.push("Workers", "review finished\nsecond line");
    // Repeated unfocused observations keep the original mark.
    assert_eq!(inbox.returned(false), None);
    inbox.push("Background", "build failed");
    assert_eq!(
        inbox.returned(true).as_deref(),
        Some("Background: build failed\nWorkers: review finished")
    );
    // The digest is shown once, and a quiet absence shows nothing.
    assert_eq!(inbox.returned(true), None);
    assert_eq!(inbox.returned(false), None);
    assert_eq!(inbox.returned(true), None);

    assert_eq!(inbox.returned(false), None);
    for i in 0..5 {
        inbox.push("Background", &format!("job {i}"));
    }
    let digest = inbox.returned(true).unwrap();
    assert!(digest.starts_with("Background: job 4\nBackground: job 3\nBackground: job 2\n"));
    assert!(digest.ends_with("+2 more; /inbox lists them"), "{digest}");
}
