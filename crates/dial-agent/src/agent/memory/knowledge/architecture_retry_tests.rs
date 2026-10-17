use super::*;

#[test]
fn failed_synthesis_releases_its_attempt_marker_for_retry() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(project.path().join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    let workspace = Workspace::new(project.path()).unwrap();
    cache.refresh(&workspace, &AtomicBool::new(false)).unwrap();
    let synthesis = cache.architecture_synthesis(8000, true).unwrap().unwrap();
    cache.mark_architecture_attempt(&synthesis).unwrap();

    let failure: Result<()> = Err(anyhow::anyhow!("invalid model response"));
    assert!(defer_architecture_attempt_on_failure(&cache, &synthesis, failure).is_err());
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_some());
}
