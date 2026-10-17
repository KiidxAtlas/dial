use super::*;
use crate::storage::image_refs::{ImageInput, register};

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), user.path().to_path_buf()).unwrap();
    (project, user, layout)
}

fn png(extra: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(extra);
    bytes
}

fn add(layout: &Layout, session: &str, bytes: &[u8]) {
    register(&layout.user_dir, session, ImageInput::Bytes(bytes), None).unwrap();
}

#[test]
fn status_counts_references_and_sessions() {
    let (_p, _u, layout) = layout();
    let a = png(b"a");
    let b = png(b"bb");
    add(&layout, "s1", &a);
    add(&layout, "s2", &b);
    add(&layout, "s2", &a);
    let out = status(&layout).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.json["count"], 2);
    assert_eq!(out.json["bytes"], (a.len() + b.len()) as u64);
    assert_eq!(out.json["sessions"], 2);
    assert_eq!(out.json["ttlHours"], IMAGE_TTL_HOURS);
    assert_eq!(out.json["expired"], 0);
    assert!(out.text.contains("References: 2"));
}

#[test]
fn doctor_healthy_empty_store() {
    let (_p, _u, layout) = layout();
    let out = doctor(&layout).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.json["healthy"], true);
    assert_eq!(out.json["entries"], 0);
}

#[test]
fn doctor_detects_corrupted_blob() {
    let (_p, _u, layout) = layout();
    let bytes = png(b"sound");
    add(&layout, "s1", &bytes);
    let sha = image_refs::list(&layout.user_dir).unwrap()[0]
        .sha256
        .clone();
    let blob = image_refs::store_dir(&layout.user_dir).join(&sha);
    fs::write(&blob, png(b"tampered")).unwrap();
    let out = doctor(&layout).unwrap();
    assert_eq!(out.code, 1);
    assert_eq!(out.json["healthy"], false);
    assert_eq!(out.json["problems"].as_array().unwrap().len(), 1);
    assert!(out.text.contains("unhealthy: 1 problem(s)"));
}

#[test]
fn purge_dry_run_leaves_blobs() {
    let (_p, _u, layout) = layout();
    let bytes = png(b"keep");
    add(&layout, "s1", &bytes);
    let out = purge(&layout, true, false).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.json["applied"], false);
    assert_eq!(out.json["selected"], 1);
    assert_eq!(out.json["deleted"], 0);
    assert!(
        out.text
            .starts_with("Dry run: 1 reference(s) would be purged")
    );
    let sha = image_refs::list(&layout.user_dir).unwrap()[0]
        .sha256
        .clone();
    assert!(image_refs::resolve(&layout.user_dir, &sha).is_ok());
}

#[test]
fn purge_apply_all_removes_blobs() {
    let (_p, _u, layout) = layout();
    add(&layout, "s1", &png(b"gone"));
    let sha = image_refs::list(&layout.user_dir).unwrap()[0]
        .sha256
        .clone();
    let out = purge(&layout, true, true).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.json["deleted"], 1);
    assert!(out.text.starts_with("Purged 1 of 1 reference(s)"));
    assert!(image_refs::list(&layout.user_dir).unwrap().is_empty());
    assert!(image_refs::resolve(&layout.user_dir, &sha).is_err());
}

#[test]
fn probe_reads_all_blobs() {
    let (_p, _u, layout) = layout();
    add(&layout, "s1", &png(b"one"));
    add(&layout, "s1", &png(b"two"));
    let out = probe(&layout, Some(5)).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.json["ok"], true);
    assert_eq!(out.json["blobs"], 2);
    assert_eq!(out.json["timeoutMs"], 5000);
}

#[test]
fn probe_fails_when_blob_missing() {
    let (_p, _u, layout) = layout();
    add(&layout, "s1", &png(b"lost"));
    let sha = image_refs::list(&layout.user_dir).unwrap()[0]
        .sha256
        .clone();
    fs::remove_file(image_refs::store_dir(&layout.user_dir).join(sha)).unwrap();
    let out = probe(&layout, Some(5)).unwrap();
    assert_eq!(out.code, 1);
    assert_eq!(out.json["ok"], false);
    assert!(out.json["detail"].as_str().unwrap().contains("missing"));
}

#[test]
fn bounded_reports_timeout_and_result() {
    let fast = bounded(Duration::from_secs(5), || 7).unwrap();
    assert_eq!(fast, 7);
    let slow = bounded(Duration::from_millis(20), || {
        thread::sleep(Duration::from_millis(500));
        1
    });
    assert!(slow.unwrap_err().starts_with("timed out after 20ms"));
}

#[test]
fn run_uses_layout_when_no_dir_given() {
    let (_p, _u, layout) = layout();
    let code = run(
        &layout,
        ImagesArgs {
            action: None,
            json: true,
            apply: false,
            all: false,
            dir: None,
            timeout: None,
        },
    )
    .unwrap();
    assert_eq!(code, 0);
}

#[test]
fn format_bytes_units() {
    assert_eq!(format_bytes(12), "12 B");
    assert_eq!(format_bytes(2048), "2.0 KiB");
    assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
}
