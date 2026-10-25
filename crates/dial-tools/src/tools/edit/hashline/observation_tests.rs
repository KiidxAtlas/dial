use super::*;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

fn observed(ws: &Workspace, name: &str, text: &str) -> PathBuf {
    let path = ws.root().join(name);
    fs::write(&path, text).unwrap();
    ws.store().record(&path, &normalize(text), &[(1, 1)]);
    path
}

#[test]
fn native_publication_and_normalized_content_do_not_notify() {
    let (_dir, ws) = workspace();
    let path = observed(&ws, "a.txt", "old\n");
    fs::write(&path, "new\n").unwrap();
    ws.store().record_all(&path, "new\n");
    assert!(ws.changed_observations().is_empty());
    fs::write(&path, "\u{feff}new\r\n").unwrap();
    assert!(ws.changed_observations().is_empty());
    fs::remove_file(&path).unwrap();
    ws.store().forget(&path);
    assert!(ws.changed_observations().is_empty());
}

#[test]
fn external_versions_and_deletion_notify_once_without_granting_witnesses() {
    let (_dir, ws) = workspace();
    let path = observed(&ws, "a.txt", "old\n");
    for text in ["changed\n", "another\n"] {
        fs::write(&path, text).unwrap();
        assert_eq!(ws.changed_observations(), vec!["a.txt"]);
        assert!(ws.changed_observations().is_empty());
        assert!(ws.store().check_ranges(&path, text, &[(1, 1)]).is_err());
        assert_eq!(ws.store().files[&path].len(), 1);
    }
    fs::remove_file(&path).unwrap();
    assert_eq!(ws.changed_observations(), vec!["a.txt"]);
    assert!(ws.changed_observations().is_empty());
}

#[test]
fn reread_and_recovery_reset_recognition() {
    let (_dir, ws) = workspace();
    let path = observed(&ws, "a.txt", "old\n");
    fs::write(&path, "new\n").unwrap();
    assert_eq!(ws.changed_observations(), vec!["a.txt"]);
    ws.store().record(&path, "new\n", &[(1, 1)]);
    assert!(ws.changed_observations().is_empty());
    fs::write(&path, "old\n").unwrap();
    assert_eq!(ws.changed_observations(), vec!["a.txt"]);
    fs::write(&path, "new\n").unwrap();
    assert!(ws.changed_observations().is_empty());
    fs::write(&path, "old\n").unwrap();
    assert_eq!(ws.changed_observations(), vec!["a.txt"]);
}

#[test]
fn unseen_private_notebook_and_large_files_are_not_exposed() {
    let (_dir, ws) = workspace();
    fs::write(ws.root().join("unseen.txt"), "secret").unwrap();
    let private = observed(&ws, "private.txt", "old");
    let ws = ws.with_protected_paths(std::slice::from_ref(&private));
    fs::write(&private, "secret").unwrap();
    let notebook = observed(&ws, "cells.ipynb", "old");
    fs::write(notebook, "new").unwrap();
    let large = observed(&ws, "large.txt", "old");
    fs::write(large, "x".repeat(OBSERVATION_FILE_BYTES + 1)).unwrap();
    assert!(ws.changed_observations().is_empty());
    assert!(ws.store().observations.is_empty());
}

#[test]
fn notice_count_and_round_robin_are_bounded() {
    let (_dir, ws) = workspace();
    let count = OBSERVATION_FILES + 10;
    for i in 0..count {
        let path = observed(&ws, &format!("{i}.txt"), "old");
        fs::write(path, "new").unwrap();
    }
    let mut all = std::collections::HashSet::new();
    for _ in 0..count * 2 {
        let changed = ws.changed_observations();
        assert!(changed.len() <= OBSERVATION_NOTICES);
        for path in changed {
            assert!(
                all.insert(path),
                "duplicate notice for unchanged disk version"
            );
        }
        if all.len() == count {
            break;
        }
    }
    assert_eq!(all.len(), count);
}

#[cfg(unix)]
#[test]
fn replaced_symlink_never_reads_outside_or_private_content() {
    let (_dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let path = observed(&ws, "a.txt", "old");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&secret, &path).unwrap();
    assert!(ws.changed_observations().is_empty());
    assert!(ws.store().observations.is_empty());
}

#[test]
fn full_digest_distinguishes_short_tag_collisions() {
    let (_dir, ws) = workspace();
    let mut tags = HashMap::new();
    let (first, second) = (0..100_000)
        .find_map(|i| {
            let text = format!("version {i}");
            tags.insert(tag_of(&text), text.clone())
                .map(|prior| (prior, text))
        })
        .expect("fixture must find a 16-bit collision");
    assert_eq!(tag_of(&first), tag_of(&second));
    let path = observed(&ws, "a.txt", &first);
    fs::write(path, second).unwrap();
    assert_eq!(ws.changed_observations(), vec!["a.txt"]);
}
