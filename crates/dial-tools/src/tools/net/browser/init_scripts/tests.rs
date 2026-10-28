use super::*;
use std::path::PathBuf;

fn no_paths(_: &str) -> Result<PathBuf, String> {
    Err("paths are not expected in this test".into())
}

#[test]
fn add_requires_exactly_one_source() {
    assert!(parse("add_init_script", &json!({}), no_paths).is_err());
    assert!(
        parse(
            "add_init_script",
            &json!({"source": "1", "path": "a.js"}),
            no_paths
        )
        .is_err()
    );
}

#[test]
fn inline_source_must_be_non_empty_and_bounded() {
    assert!(parse("add_init_script", &json!({"source": "  "}), no_paths).is_err());
    let too_big = "a".repeat(INIT_SCRIPT_MAX_BYTES + 1);
    assert!(parse("add_init_script", &json!({"source": too_big}), no_paths).is_err());
    let ok = parse(
        "add_init_script",
        &json!({"source": "window.x = 1"}),
        no_paths,
    )
    .unwrap();
    assert_eq!(ok["source"], "window.x = 1");
}

#[test]
fn path_source_is_read_from_resolved_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("init.js");
    std::fs::write(&file, "globalThis.loaded = true;").unwrap();
    let resolved = file.clone();
    let parsed = parse("add_init_script", &json!({"path": "init.js"}), move |_| {
        Ok::<_, String>(resolved.clone())
    })
    .unwrap();
    assert_eq!(parsed["source"], "globalThis.loaded = true;");
}

#[test]
fn oversized_or_binary_files_are_rejected_without_echoing_content() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.js");
    std::fs::write(&big, vec![b'a'; INIT_SCRIPT_MAX_BYTES + 1]).unwrap();
    let error = read_script_file(&big).unwrap_err();
    assert!(error.contains("exceeds"));
    let binary = dir.path().join("bin.js");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    let error = read_script_file(&binary).unwrap_err();
    assert!(error.contains("UTF-8"));
}

#[test]
fn remove_requires_a_bounded_script_id() {
    assert!(parse("remove_init_script", &json!({}), no_paths).is_err());
    assert!(parse("remove_init_script", &json!({"script_id": ""}), no_paths).is_err());
    let parsed = parse(
        "remove_init_script",
        &json!({"script_id": "init-1"}),
        no_paths,
    )
    .unwrap();
    assert_eq!(parsed["script_id"], "init-1");
}
