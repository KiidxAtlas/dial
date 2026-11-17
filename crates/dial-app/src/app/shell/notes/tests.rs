use crate::ui::overlay::inbox::Category;

#[test]
fn notification_delivery_and_preferences_use_producer_metadata_not_titles() {
    let (_home, _project, _app, mut shell) = super::super::testkit::shell();
    for title in ["Workers", "Theme", "Notice", "Error"] {
        shell.note(title, "command result");
    }
    assert_eq!(shell.inbox.unread(), 0);
    assert_eq!(shell.status.inbox_unread, 0);
    shell.notify(Category::Error, "A harmless title", "operation failed");
    assert_eq!(shell.inbox.unread(), 1);
    assert_eq!(shell.status.inbox_unread, 1);
    assert_eq!(shell.inbox.settings_path(1), Some("ui.notify_errors"));
    shell.notify(Category::General, "Error preferences", "saved");
    assert_eq!(shell.inbox.settings_path(2), Some("ui.notify"));
}
