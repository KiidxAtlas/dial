use super::testkit::*;
use serde_json::json;

#[test]
fn absolute_paths_resolve_under_added_roots_and_relative_under_primary() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "e").unwrap();
    assert!(ws.add_root(extra.path()).unwrap());

    let inside = extra.path().join("e.txt");
    assert_eq!(
        ws.resolve(inside.to_str().unwrap()).unwrap(),
        inside.canonicalize().unwrap()
    );
    assert_eq!(ws.resolve("p.txt").unwrap(), ws.root().join("p.txt"));
    assert_eq!(ws.roots().count(), 2);
    assert_eq!(
        ws.additional_roots(),
        [extra.path().canonicalize().unwrap()]
    );
}

#[test]
fn paths_outside_every_root_are_refused() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    ws.add_root(extra.path()).unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("o.txt"), "o").unwrap();
    let err = ws
        .resolve(other.path().join("o.txt").to_str().unwrap())
        .unwrap_err();
    assert!(err.contains("outside the workspace"), "{err}");
    let escape = format!("{}/../{}", extra.path().display(), "o.txt");
    assert!(ws.resolve(&escape).is_err());
}

#[test]
fn removed_root_stops_resolving_and_primary_cannot_be_added_or_removed() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "e").unwrap();
    let file = extra.path().join("e.txt");
    assert!(ws.add_root(extra.path()).unwrap());
    assert!(!ws.add_root(extra.path()).unwrap(), "second add is a no-op");
    assert!(ws.remove_root(extra.path()).unwrap());
    assert!(!ws.remove_root(extra.path()).unwrap());
    assert!(ws.resolve(file.to_str().unwrap()).is_err());

    let root = ws.root().to_path_buf();
    assert!(ws.add_root(&root).is_err());
    assert!(ws.remove_root(&root).is_err());
}

#[test]
fn with_additional_roots_rejects_missing_directories() {
    let (_dir, ws) = workspace();
    let missing = ws.root().join("does-not-exist");
    assert!(ws.with_additional_roots(&[missing]).is_err());
}

/// `/add-dir` must make file tools work under the added directory, not just path resolution:
/// the confined edit transaction used to walk only from the primary root.
#[test]
fn file_tools_read_and_write_under_an_added_root() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "existing\n").unwrap();
    ws.add_root(extra.path()).unwrap();
    let root = extra.path().canonicalize().unwrap();

    let read = run(&ws, "read", json!({"path": root.join("e.txt")}));
    assert!(!read.is_error, "{}", read.text);
    assert!(read.text.contains("existing"), "{}", read.text);

    let created = root.join("sub/new.txt");
    let write = run(&ws, "write", json!({"path": created, "content": "made\n"}));
    assert!(!write.is_error, "{}", write.text);
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "made\n");
}
