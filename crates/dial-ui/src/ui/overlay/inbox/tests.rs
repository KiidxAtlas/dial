use super::*;

#[test]
fn repeats_reopen_read_notices_and_retain_bounded_occurrences() {
    let mut inbox = Inbox::default();
    inbox.push("Workers", "finished");
    inbox.push("Workers", "finished");
    assert_eq!(inbox.unread(), 1);
    assert_eq!(inbox.read(1), Some(("Workers", "finished")));
    inbox.push("Workers", "finished");
    assert_eq!(inbox.unread(), 1);
    assert_eq!(inbox.entries.len(), 1);
    assert_eq!(inbox.entries[0].occurrences, 3);
    assert_eq!(inbox.entries[0].id, 1, "open menu references stay valid");
    assert!(
        inbox
            .choices()
            .items
            .iter()
            .any(|i| i.detail.contains("3 occurrences"))
    );
}

#[test]
fn capacity_evicts_read_notices_first_and_reports_unread_overflow() {
    let mut inbox = Inbox::default();
    for i in 0..LIMIT {
        inbox.push("Background", &format!("job {i}"));
    }
    inbox.read(2).unwrap();
    inbox.push("Background", "next");
    assert!(inbox.entries.iter().any(|entry| entry.id == 1));
    assert!(!inbox.entries.iter().any(|entry| entry.id == 2));
    assert_eq!(inbox.unread(), LIMIT);
    inbox.push("Background", "overflow");
    assert_eq!(inbox.entries.len(), LIMIT);
    assert_eq!(inbox.overflow_unread, 1);
    assert!(inbox.choices().summary.contains("1 unread notice evicted"));
    assert_eq!(inbox.read_all(), LIMIT);
    assert_eq!(
        inbox.overflow_unread, 1,
        "mark-all must not conceal lost notices"
    );
}

#[test]
fn repeat_coalescing_has_a_fixed_time_window() {
    let mut inbox = Inbox::default();
    let now = std::time::Instant::now();
    inbox.push_at("Job", "done", Category::General, now);
    inbox.push_at("Job", "done", Category::General, now + COALESCE_WINDOW);
    assert_eq!(inbox.entries.len(), 1);
    inbox.push_at(
        "Job",
        "done",
        Category::General,
        now + COALESCE_WINDOW + std::time::Duration::from_secs(1),
    );
    assert_eq!(inbox.entries.len(), 2);
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
        assert_eq!(
            top.items[2].value, "1",
            "Enter on a notice reads it directly"
        );
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
        ["read-all", "unread", "2", "1", "notifications"],
        "one settings row; per-category preferences live on each notice's menu"
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

#[test]
fn repeated_notice_arriving_while_away_is_in_the_return_digest() {
    let mut inbox = Inbox::default();
    inbox.returned(true);
    inbox.push("Worker", "done");
    inbox.read(1).unwrap();
    inbox.returned(false);
    inbox.push("Worker", "done");
    assert_eq!(inbox.returned(true).as_deref(), Some("Worker: done"));
    assert_eq!(inbox.unread(), 1);
}

#[test]
fn unread_filter_keeps_actions_and_only_unread_notice_rows_without_reading() {
    let mut inbox = Inbox::default();
    inbox.push(
        "Read",
        "unread appears in this body but must not match the filter",
    );
    inbox.push("Unread", "new");
    inbox.read(1).unwrap();
    let filtered = inbox.choices_filtered(true);
    let values: Vec<_> = filtered
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, ["read-all", "all", "2", "notifications"]);
    assert_eq!(inbox.unread(), 1);
    let all = inbox.choices();
    assert_eq!(
        all.items[0].value, "read-all",
        "bulk action stays prominent"
    );
    assert!(all.items.iter().any(|item| item.value == "1"));
}
