use super::*;

#[cfg(unix)]
#[test]
fn bulk_import_reports_invalid_skills_and_preserves_source_identity() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    for (name, declared) in [("review", "review"), ("bad", "different")] {
        let dir = sources.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join(SKILL_FILE),
            format!("---\nname: {declared}\ndescription: Test\n---\nBody"),
        )
        .unwrap();
    }
    let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
    let reports = run(&layout, sources.path(), Scope::User, false, false).unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports[0].contains("Skipped"));
    assert_eq!(reports[1], "Imported review");
    let found = discover(&layout);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source, Scope::User);
    let replacement = other.path().join("review");
    fs::create_dir(&replacement).unwrap();
    fs::copy(
        sources.path().join("review/SKILL.md"),
        replacement.join(SKILL_FILE),
    )
    .unwrap();
    assert!(
        run(&layout, &replacement, Scope::User, false, true).unwrap()[0].contains("source differs")
    );
    std::os::unix::fs::symlink(home.path(), layout.project_dir.clone()).unwrap();
    assert!(run(&layout, sources.path(), Scope::Project, false, false).is_err());
}
