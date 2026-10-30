use super::*;

fn layout(root: &Path) -> Layout {
    Layout {
        project_root: root.into(),
        project_dir: root.join("project"),
        user_dir: root.join("user"),
        state_dir: root.join("state"),
    }
}

#[test]
fn published_state_round_trips_and_a_missing_file_is_none() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    assert_eq!(read(&layout, "session").unwrap(), None);
    let snapshot = Snapshot {
        version: VERSION,
        updated_ms: 42,
        busy: true,
        jobs: vec![Job {
            kind: JobKind::Command,
            name: "Update".into(),
            running: true,
            output: vec!["$ make update".into()],
            ..Job::default()
        }],
        ..Snapshot::default()
    };
    publish(&layout, "session", &snapshot).unwrap();
    assert_eq!(read(&layout, "session").unwrap(), Some(snapshot));
}

#[test]
fn state_from_another_version_is_refused_with_a_reason() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let snapshot = Snapshot {
        version: VERSION + 1,
        ..Snapshot::default()
    };
    publish(&layout, "session", &snapshot).unwrap();
    let error = read(&layout, "session").unwrap_err().to_string();
    assert!(error.contains("same Dial build"), "{error}");
}

#[test]
fn requests_drain_once_in_order() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    assert!(drain(&layout, "session").unwrap().is_empty());
    let sent = [
        Request::CancelWorker { id: "w1".into() },
        Request::StopProcess {
            name: "server".into(),
        },
        Request::MessagePeer {
            to: "abc".into(),
            text: "please hold off on src/lib.rs".into(),
        },
    ];
    for request in &sent {
        send(&layout, "session", request).unwrap();
    }
    let drained: Vec<Request> = drain(&layout, "session")
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(drained, sent);
    assert!(drain(&layout, "session").unwrap().is_empty());
}

#[test]
fn malformed_requests_are_removed_and_reported_and_foreign_files_ignored() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let dir = inbox(&layout, "session").unwrap();
    atomic::private_dir(&dir).unwrap();
    let bad = dir.join(format!("{:020}-{:010}-{:06}.json", 1, 1, 1));
    atomic::write_private(&bad, br#"{"action":"approve_everything"}"#).unwrap();
    let foreign = dir.join("notes.json");
    atomic::write_private(&foreign, b"{}").unwrap();
    let drained = drain(&layout, "session").unwrap();
    assert_eq!(drained.len(), 1);
    assert!(drained[0].is_err());
    assert!(!bad.exists());
    assert!(foreign.exists());
}

#[test]
fn invalid_requests_are_refused_before_queueing() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    for request in [
        Request::MessagePeer {
            to: "abc".into(),
            text: "  ".into(),
        },
        Request::MessagePeer {
            to: "abc".into(),
            text: "x".repeat(MAX_MESSAGE + 1),
        },
        Request::CancelWorker { id: "".into() },
        Request::StopProcess {
            name: "a\nb".into(),
        },
    ] {
        assert!(send(&layout, "session", &request).is_err(), "{request:?}");
    }
    assert!(drain(&layout, "session").unwrap().is_empty());
}

#[test]
fn bounded_cuts_on_a_character_boundary() {
    assert_eq!(bounded("short", 10), "short");
    let cut = bounded(&"é".repeat(10), 5);
    assert_eq!(cut, "éé…");
}
