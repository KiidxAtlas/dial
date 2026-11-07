use super::*;
use crate::agent::window::context::{
    DIR_LISTING_CAP, MAX_REFS, REF_FILE_CAP, REF_TOTAL_CAP, has_attachment_candidates,
    prepare_message, remove_attachment_reference,
};
use std::fs;
use std::sync::atomic::AtomicBool;

fn prepare(root: &std::path::Path, text: &str) -> PreparedMessage {
    prepare_message(root, text, &AtomicBool::new(false))
}

#[test]
fn quoted_duplicate_references_report_only_the_retained_block() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("my notes.txt"), "hello </file> 世界").unwrap();
    let text = "read @\"my notes.txt\" and @'my notes.txt'";
    let prepared = prepare(root.path(), text);
    assert_eq!(prepared.attachments.len(), 1);
    let item = &prepared.attachments[0];
    assert_eq!(item.reference, "my notes.txt");
    assert_eq!(item.kind, AttachmentKind::File);
    assert_eq!(item.state, AttachmentState::Included);
    let content = prepared.message["content"].as_str().unwrap();
    assert_eq!(content.matches("<file ").count(), 1);
    assert!(content.contains("<\\/file>"));
    assert_eq!(item.text_bytes, content.len() - text.len() - 2);
    assert_eq!(
        prepared.summary(),
        format!(
            "1 text attachment(s) · ~{} text tokens",
            item.text_bytes.div_ceil(4)
        )
    );
    assert!(prepared.issues().is_empty());
}

#[test]
fn missing_binary_and_escaping_paths_are_skipped_not_counted_as_text() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("binary.bin"), [0, 1, 255]).unwrap();
    for reference in [
        "missing.txt",
        "binary.bin",
        "../outside.txt",
        "/outside.txt",
    ] {
        let text = format!("read @{reference}");
        let prepared = prepare(root.path(), &text);
        assert_eq!(prepared.message["content"], text);
        assert_eq!(prepared.attachments.len(), 1, "{reference}");
        assert_eq!(prepared.attachments[0].state, AttachmentState::Skipped);
        assert_eq!(prepared.attachments[0].text_bytes, 0);
        assert_eq!(
            prepared.issues().len(),
            1,
            "warnings and metadata must not double count"
        );
        assert_eq!(prepared.summary(), "1 attachment issue(s)");
    }
    for text in [
        "a@b.com",
        "hi @someone",
        "@\"unterminated.txt",
        "no references",
    ] {
        let prepared = prepare(root.path(), text);
        assert_eq!(prepared.message["content"], text);
        assert!(prepared.attachments.is_empty());
        assert!(prepared.issues().is_empty());
        assert!(prepared.summary().is_empty());
    }
}

#[test]
fn clipped_text_and_total_budget_metadata_match_admitted_blocks() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("large.txt"), "x".repeat(REF_FILE_CAP + 1)).unwrap();
    let prepared = prepare(root.path(), "@large.txt");
    let item = &prepared.attachments[0];
    assert_eq!(item.state, AttachmentState::Truncated);
    assert!(
        prepared.message["content"]
            .as_str()
            .unwrap()
            .contains("truncated at 64 KiB")
    );
    assert_eq!(
        item.text_bytes,
        prepared.message["content"].as_str().unwrap().len() - "@large.txt\n\n".len()
    );
    assert_eq!(prepared.issues().len(), 1);
    // Legacy expansion has no warning for file clipping; issues must still expose it.
    assert!(prepared.warnings.is_empty());

    let mut text = String::new();
    for index in 0..5 {
        fs::write(root.path().join(format!("{index}.txt")), "y".repeat(60_000)).unwrap();
        text.push_str(&format!("@{index}.txt "));
    }
    let prepared = prepare(root.path(), &text);
    let retained: Vec<_> = prepared
        .attachments
        .iter()
        .filter(|item| item.state != AttachmentState::Skipped)
        .collect();
    assert_eq!(retained.len(), 4);
    assert_eq!(prepared.attachments[4].state, AttachmentState::Skipped);
    assert_eq!(prepared.attachments[4].text_bytes, 0);
    assert_eq!(prepared.issues().len(), 1);
    assert!(prepared.issues()[0].contains("in total"));
    let bytes: usize = retained.iter().map(|item| item.text_bytes).sum();
    assert!(bytes <= REF_TOTAL_CAP);
    let content = prepared.message["content"].as_str().unwrap();
    assert_eq!(content.matches("<file ").count(), retained.len());
    assert_eq!(content.len() - text.len(), bytes + 2 * retained.len());
}

#[test]
fn directory_omission_keeps_its_retained_bytes_and_one_issue() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("many")).unwrap();
    for index in 0..=DIR_LISTING_CAP {
        fs::write(root.path().join("many").join(format!("{index}.txt")), "").unwrap();
    }
    let text = "@many/";
    let prepared = prepare(root.path(), text);
    assert_eq!(prepared.attachments.len(), 1);
    let item = &prepared.attachments[0];
    assert_eq!(item.kind, AttachmentKind::Directory);
    assert_eq!(item.state, AttachmentState::Truncated);
    let content = prepared.message["content"].as_str().unwrap();
    assert!(content.contains("directory listing omitted"));
    assert_eq!(item.text_bytes, content.len() - text.len() - 2);
    assert_eq!(prepared.issues().len(), 1);
    assert!(prepared.summary().starts_with("1 text attachment(s)"));
}

#[test]
fn text_and_pasted_image_candidate_limits_are_visible() {
    let root = tempfile::tempdir().unwrap();
    let mut text = String::new();
    for index in 0..=MAX_REFS {
        fs::write(root.path().join(format!("{index}.txt")), "x").unwrap();
        text.push_str(&format!("@{index}.txt "));
    }
    let prepared = prepare(root.path(), &text);
    assert_eq!(prepared.attachments.len(), MAX_REFS);
    assert_eq!(
        prepared.message["content"]
            .as_str()
            .unwrap()
            .matches("<file ")
            .count(),
        MAX_REFS
    );
    assert_eq!(prepared.issues().len(), 1);

    let mut images = String::new();
    for index in 0..=MAX_REFS {
        fs::write(
            root.path().join(format!("{index}.png")),
            b"\x89PNG\r\n\x1a\nfixture",
        )
        .unwrap();
        images.push_str(&format!("./{index}.png "));
    }
    let prepared = prepare(root.path(), &images);
    assert_eq!(prepared.attachments.len(), MAX_REFS);
    assert_eq!(
        prepared.message["dial_images"].as_array().unwrap().len(),
        MAX_REFS
    );
    assert_eq!(
        prepared.issues().len(),
        1,
        "pasted image overflow must not silently disappear"
    );
}

#[test]
fn image_metadata_matches_references_and_source_size_and_path_refusals() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("Screenshot 1.png");
    fs::write(&path, b"\x89PNG\r\n\x1a\nfixture").unwrap();
    let text = format!("@\"{}\" and \"{}\"", path.display(), path.display());
    let prepared = prepare(root.path(), &text);
    assert_eq!(prepared.attachments.len(), 1);
    assert_eq!(prepared.attachments[0].kind, AttachmentKind::Image);
    assert_eq!(prepared.attachments[0].text_bytes, 0);
    assert_eq!(prepared.message["dial_images"].as_array().unwrap().len(), 1);
    assert_eq!(prepared.summary(), "1 image(s) · image tokens vary");
    assert!(prepared.issues().is_empty());

    fs::write(root.path().join("invalid.png"), b"not an image").unwrap();
    let huge = fs::File::create(root.path().join("huge.png")).unwrap();
    huge.set_len(crate::llm::images::MAX_IMAGE_SOURCE_BYTES + 1)
        .unwrap();
    for text in [
        "@missing.png",
        "@invalid.png",
        "@huge.png",
        "@../outside.png",
    ] {
        let prepared = prepare(root.path(), text);
        assert!(prepared.message.get("dial_images").is_none());
        assert_eq!(prepared.attachments.len(), 1);
        assert_eq!(prepared.attachments[0].kind, AttachmentKind::Image);
        assert_eq!(prepared.attachments[0].state, AttachmentState::Skipped);
        assert_eq!(prepared.issues().len(), 1);
        assert_eq!(prepared.summary(), "1 attachment issue(s)");
    }
}

#[test]
fn cancellation_is_visible_for_text_refs_and_pasted_images_without_reading_them() {
    let root = tempfile::tempdir().unwrap();
    for text in ["@missing.txt", "./missing.png", "@missing.png ./other.png"] {
        let prepared = prepare_message(root.path(), text, &AtomicBool::new(true));
        assert_eq!(
            prepared.message,
            serde_json::json!({"role":"user", "content":text})
        );
        assert!(prepared.attachments.is_empty());
        assert_eq!(prepared.issues().len(), 1);
        assert!(prepared.issues()[0].contains("interrupted"));
    }
}

#[test]
fn removal_uses_exact_parser_spans_and_preserves_prose_and_punctuation() {
    let cases = [
        ("see @a.txt, then @a.txt!", "a.txt", Some("see , then !")),
        (
            "α @\"my notes.txt\" β @'my notes.txt'.",
            "my notes.txt",
            Some("α  β ."),
        ),
        (
            "see \"/tmp/my screenshot.png\", next",
            "/tmp/my screenshot.png",
            Some("see , next"),
        ),
        ("./image.png; @./image.png!", "./image.png", Some("; !")),
        (
            "`./image.png` and './image.png'",
            "./image.png",
            Some(" and "),
        ),
        (
            "a@b.com @b.com @b.com.txt",
            "b.com",
            Some("a@b.com  @b.com.txt"),
        ),
        (
            "@\"/tmp/Screenshot @1.png\"",
            "/tmp/Screenshot @1.png",
            Some(""),
        ),
        ("@a.txt @ab.txt", "ab", None),
        (
            "ordinary a.txt and https://example.com/a.png",
            "a.txt",
            None,
        ),
        ("@\"unterminated.txt", "unterminated.txt", None),
        ("", "", None),
    ];
    for (text, reference, expected) in cases {
        assert_eq!(
            remove_attachment_reference(text, reference).as_deref(),
            expected,
            "{text:?}"
        );
    }
    assert!(has_attachment_candidates("@\"my notes.txt\""));
    assert!(has_attachment_candidates("\"/tmp/my screenshot.png\""));
    assert!(!has_attachment_candidates(
        "a@b.com and https://example.com/a.png"
    ));
}
