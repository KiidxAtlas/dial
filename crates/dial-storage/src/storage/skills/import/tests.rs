use super::*;
#[test]
fn import_preview_update_and_local_edit_protection() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("review");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join(SKILL_FILE),
        "---\nname: review\ndescription: Review code\n---\nReview carefully.",
    )
    .unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
    let run_import = |dry, update| {
        run(&layout, &source, Scope::Project, dry, update)
            .unwrap()
            .join("\n")
    };
    assert!(run_import(true, false).contains("Would import"));
    assert!(!layout.project_dir.exists());
    assert!(run_import(false, false).contains("Imported"));
    assert_eq!(discover(&layout).len(), 1);
    assert!(run_import(false, false).contains("already installed"));
    fs::write(source.join("reference.md"), "new reference").unwrap();
    assert!(run_import(false, true).contains("Imported"));
    let installed = layout.skills_dirs()[0].join("review/reference.md");
    fs::write(&installed, "my edits").unwrap();
    assert!(run_import(false, true).contains("local changes"));
    assert_eq!(fs::read_to_string(installed).unwrap(), "my edits");
}

#[cfg(unix)]
#[test]
fn rejects_links_and_oversized_bundles() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("review");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join(SKILL_FILE),
        "---\ndescription: Review\n---\nBody",
    )
    .unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
    std::os::unix::fs::symlink("SKILL.md", source.join("link")).unwrap();
    assert!(run(&layout, &source, Scope::User, false, false).unwrap()[0].contains("symbolic link"));
    fs::remove_file(source.join("link")).unwrap();
    fs::write(source.join("large"), vec![0; MAX_READ_BYTES + 1]).unwrap();
    assert!(run(&layout, &source, Scope::User, false, false).unwrap()[0].contains("64 KiB"));
    assert!(discover(&layout).is_empty());
}
