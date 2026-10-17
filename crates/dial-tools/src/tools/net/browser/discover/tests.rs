use super::*;

fn build(root: &Path, revision: u32, complete: bool) -> PathBuf {
    let dir = root.join(format!("chromium-{revision}"));
    let exe = dir.join("chrome-test").join(BUILD_EXECUTABLES[0]);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "").unwrap();
    if complete {
        std::fs::write(dir.join(COMPLETE_MARKER), "").unwrap();
    }
    exe
}

#[test]
fn newest_complete_playwright_build_wins_over_system_browser() {
    let cache = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    build(cache.path(), 1237, true);
    let newest = build(private.path(), 1248, true);
    // An interrupted download (no marker) and a headless shell are never picked.
    build(cache.path(), 1300, false);
    let shell = cache.path().join("chromium_headless_shell-1400");
    std::fs::create_dir_all(&shell).unwrap();
    std::fs::write(shell.join(COMPLETE_MARKER), "").unwrap();
    let system = cache.path().join("system-chrome");
    std::fs::write(&system, "").unwrap();
    let roots = [private.path().to_path_buf(), cache.path().to_path_buf()];
    assert_eq!(find(&roots, &[system]), Some(newest));
}

#[test]
fn falls_back_to_first_installed_system_browser() {
    let dir = tempfile::tempdir().unwrap();
    build(dir.path(), 1300, false);
    let edge = dir.path().join("msedge");
    std::fs::write(&edge, "").unwrap();
    let roots = [dir.path().to_path_buf(), dir.path().join("missing")];
    assert_eq!(
        find(&roots, &[dir.path().join("chrome"), edge.clone()]),
        Some(edge)
    );
    assert_eq!(find(&roots, &[dir.path().join("chrome")]), None);
}
