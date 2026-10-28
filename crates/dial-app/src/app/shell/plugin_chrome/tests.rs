use super::*;
use crate::ui::composer::CommandKind;
use std::time::{Duration, Instant};

fn suggestion(value: &str, label: &str, description: &str) -> Suggestion {
    Suggestion {
        value: value.to_owned(),
        label: label.to_owned(),
        description: description.to_owned(),
    }
}

#[test]
fn slow_lookup_does_not_block_the_caller() {
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    spawn_lookup(tx, "dep".to_owned(), |query| {
        std::thread::sleep(Duration::from_millis(400));
        vec![suggestion(&format!("{query}loy"), "deploy", "ship it")]
    });
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "spawn waited on the plugin"
    );
    let (query, entries) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("answer arrives after the lookup");
    assert_eq!(query, "dep");
    assert_eq!(entries[0].name, "deploy");
}

#[test]
fn hung_lookup_never_delivers_and_never_blocks() {
    let (tx, rx) = mpsc::channel::<Answer>();
    let started = Instant::now();
    spawn_lookup(tx, "x".to_owned(), |_| {
        std::thread::sleep(Duration::from_secs(30));
        Vec::new()
    });
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn suggestions_become_extension_commands_without_slash() {
    let entries = suggestion_entries(vec![
        suggestion("/deploy", "Deploy", "ship the branch"),
        suggestion("notes", "", "open notes"),
        suggestion("ci", "CI", ""),
    ]);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "deploy");
    assert_eq!(entries[0].description, "Deploy · ship the branch");
    assert_eq!(entries[0].kind, CommandKind::Extension);
    assert_eq!(entries[1].description, "open notes");
    assert_eq!(entries[2].description, "CI");
    assert!(entries.iter().all(|e| e.args_hint.is_none()));
}

#[test]
fn slots_map_by_placement_and_skip_nothing_visible() {
    let slots = vec![
        (
            "a".to_owned(),
            UiSlot {
                slot: Slot::Status,
                lines: vec!["mail 3".to_owned()],
            },
        ),
        (
            "b".to_owned(),
            UiSlot {
                slot: Slot::Title,
                lines: vec!["Release".to_owned()],
            },
        ),
    ];
    let mapped = slots_from(&slots);
    assert_eq!(mapped.title(), Some("Release"));
    assert!(
        mapped
            .status_row(80, &crate::ui::theme::Theme::dark())
            .is_some()
    );
    assert!(
        mapped
            .header_rows(80, &crate::ui::theme::Theme::dark())
            .is_empty()
    );
}
