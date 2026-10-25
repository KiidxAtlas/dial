use super::*;
use crate::runtime::tool_output::ToolOutput;

fn layout(root: &std::path::Path) -> Layout {
    Layout {
        project_root: root.into(),
        project_dir: root.join("project"),
        user_dir: root.join("user"),
        state_dir: root.join("state"),
    }
}
fn start(layout: &Layout, session: &str) {
    append(
        layout,
        session,
        Event::Text {
            title: "Start".into(),
            body: "Ready".into(),
        },
    )
    .unwrap();
}
fn events(layout: &Layout, session: &str) -> Vec<Event> {
    let bytes = atomic::read_private(&directory(layout, session).unwrap().join("events.jsonl"))
        .unwrap()
        .unwrap();
    bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect()
}

#[test]
fn feed_is_opt_in_and_follows_current_session() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let output = ToolOutput::ok("change preview\n-old\n+new".into())
        .with_changes([PathBuf::from("file.rs")]);
    tool_result(&layout, "first", "edit", &output).unwrap();
    assert!(!enabled(&layout, "first"));
    start(&layout, "first");
    tool_result(&layout, "first", "edit", &output).unwrap();
    tool_result(&layout, "second", "edit", &output).unwrap();
    assert_eq!(events(&layout, "first").len(), 2);
    assert!(!enabled(&layout, "second"));
    match &events(&layout, "first")[1] {
        Event::Text { body, .. } => assert!(body.contains("file.rs\nchange preview\n-old\n+new")),
        _ => panic!("expected edit preview"),
    }
}

#[test]
fn advisory_without_changed_paths_is_forwarded_but_not_promoted_to_verification() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    start(&layout, "session");
    let output = ToolOutput::ok("[LSP advisory, not verification] 2 new error(s) after this edit\nfile.rs:1: bad type\nfile.rs:2: missing name".into());
    tool_result(&layout, "session", "edit", &output).unwrap();
    match &events(&layout, "session")[1] {
        Event::Text { body, .. } => {
            assert!(body.contains("not verification: 2 new error(s)"));
            assert!(body.contains("file.rs:2: missing name"));
        }
        _ => panic!("expected advisory"),
    }
}

#[test]
fn reader_preserves_partial_events_across_eof_and_never_replays_them() {
    let mut pending = Vec::new();
    let mut first = std::io::Cursor::new(b"{\"kind\":\"text\",\"title\":\"Edit\",");
    assert!(next_event(&mut first, &mut pending).unwrap().is_none());
    let mut rest = std::io::Cursor::new(
        b"\"body\":\"+new\"}\n{\"kind\":\"text\",\"title\":\"Next\",\"body\":\"done\"}\n",
    );
    assert!(
        matches!(next_event(&mut rest, &mut pending).unwrap(), Some(Event::Text { title, body }) if title == "Edit" && body == "+new")
    );
    assert!(
        matches!(next_event(&mut rest, &mut pending).unwrap(), Some(Event::Text { title, .. }) if title == "Next")
    );
    assert!(next_event(&mut rest, &mut pending).unwrap().is_none());
    assert!(pending.is_empty());
}

#[test]
fn invalid_and_oversized_events_are_errors() {
    assert!(next_event(&mut std::io::Cursor::new(b"not json\n"), &mut Vec::new()).is_err());
    assert!(
        next_event(
            &mut std::io::Cursor::new(vec![b'x'; MAX_EVENT + 1]),
            &mut Vec::new()
        )
        .is_err()
    );
}

#[test]
fn session_and_image_names_cannot_escape_private_storage_or_panic_on_unicode() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    for session in ["", "..", "../outside", "/tmp", "a/b", "a\\b", "a\n", "a:b"] {
        assert!(directory(&layout, session).is_err(), "{session:?}");
    }
    for file in [
        "../outside",
        "file.image",
        &format!("{}é.image", "a".repeat(63)),
    ] {
        assert!(!image_name(file));
    }
    assert!(image_name(&format!("{}.image", "a".repeat(64))));
}

#[test]
fn attachment_requires_live_process_identity() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    start(&layout, "session");
    assert!(!attached(&layout, "session"));
    let dir = directory(&layout, "session").unwrap();
    atomic::write_private(
        &dir.join("viewer.json"),
        &serde_json::to_vec(&ProcId::current().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(attached(&layout, "session"));
    atomic::write_private(&dir.join("viewer.json"), br#"{"pid":2147483647,"start":1}"#).unwrap();
    assert!(!attached(&layout, "session"));
}

#[test]
fn large_unicode_previews_remain_valid_bounded_json() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    append(
        &layout,
        "session",
        Event::Text {
            title: "é".repeat(2000),
            body: "🦀\u{1b}".repeat(50000),
        },
    )
    .unwrap();
    let result = events(&layout, "session");
    assert!(
        matches!(&result[0], Event::Text { body, .. } if body.len() < 17 * 1024 && body.contains("[preview truncated]"))
    );
}

#[cfg(unix)]
#[test]
fn append_rejects_symlink_and_hardlink_targets() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let dir = directory(&layout, "session").unwrap();
    atomic::private_dir(&dir).unwrap();
    let outside = root.path().join("outside");
    std::fs::write(&outside, "unchanged").unwrap();
    let path = dir.join("events.jsonl");
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(
        append(
            &layout,
            "session",
            Event::Text {
                title: "no".into(),
                body: "no".into()
            }
        )
        .is_err()
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&outside, &path).unwrap();
    assert!(
        append(
            &layout,
            "session",
            Event::Text {
                title: "no".into(),
                body: "no".into()
            }
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
}

fn screenshot(root: &std::path::Path) -> serde_json::Value {
    let image = image::RgbaImage::from_pixel(160, 90, image::Rgba([42, 160, 210, 255]));
    image.save(root.join("screen.png")).unwrap();
    crate::llm::images::attachment(root, std::path::Path::new("screen.png")).unwrap()
}

#[test]
fn screenshots_are_verified_copied_and_only_replace_inline_when_attached() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let reference = screenshot(root.path());
    let (thumbnail, warning) = preview_image(&layout, "session", &reference, false).unwrap();
    assert!(thumbnail.is_some() && warning.is_none());
    assert!(!enabled(&layout, "session"));
    start(&layout, "session");
    let (thumbnail, warning) = preview_image(&layout, "session", &reference, false).unwrap();
    assert!(thumbnail.is_some() && warning.is_none());
    let dir = directory(&layout, "session").unwrap();
    atomic::write_private(
        &dir.join("viewer.json"),
        &serde_json::to_vec(&ProcId::current().unwrap()).unwrap(),
    )
    .unwrap();
    let (thumbnail, warning) = preview_image(&layout, "session", &reference, false).unwrap();
    assert!(thumbnail.is_none() && warning.is_none());
    let Event::Image { file, .. } = &events(&layout, "session")[1] else {
        panic!("expected screenshot");
    };
    assert_eq!(
        std::fs::read(dir.join(file)).unwrap(),
        std::fs::read(root.path().join("screen.png")).unwrap()
    );
    std::fs::write(root.path().join("screen.png"), "changed after capture").unwrap();
    assert!(image(&layout, "session", &reference).is_err());
}

#[cfg(unix)]
#[test]
fn feed_failure_keeps_the_inline_screenshot() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let reference = screenshot(root.path());
    start(&layout, "session");
    let dir = directory(&layout, "session").unwrap();
    let image_file = dir.join(format!("{}.image", reference["sha256"].as_str().unwrap()));
    std::os::unix::fs::symlink(root.path().join("screen.png"), image_file).unwrap();
    atomic::write_private(
        &dir.join("viewer.json"),
        &serde_json::to_vec(&ProcId::current().unwrap()).unwrap(),
    )
    .unwrap();
    let (thumbnail, warning) = preview_image(&layout, "session", &reference, false).unwrap();
    assert!(thumbnail.is_some());
    assert!(warning.is_some());
}
