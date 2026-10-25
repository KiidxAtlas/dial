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

#[test]
fn lock_root_waits_out_a_briefly_held_lock_but_not_a_held_one() {
    let root = tempfile::tempdir().unwrap();
    // A separate open file description conflicts like another process (or a just-spawned child
    // that still shares the descriptor) would.
    let holder = lock_root(root.path(), ".import.lock", "first").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(holder);
    });
    let started = std::time::Instant::now();
    let second = lock_root(root.path(), ".import.lock", "busy").unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    release.join().unwrap();
    let error = lock_root(
        root.path(),
        ".import.lock",
        "another skill import is running",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("another skill import is running"), "{error}");
    drop(second);
}

#[test]
fn curated_workflows_roundtrip_as_portable_skills_and_protect_local_edits() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/workflows");
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
    let destination = tempfile::tempdir().unwrap();
    let destination_home = tempfile::tempdir().unwrap();
    let second = Layout::with_user_dir(destination.path(), destination_home.path().into()).unwrap();
    for name in ["review", "pack-review"] {
        let source = fixtures.join(name);
        let original = fs::read(source.join(SKILL_FILE)).unwrap();
        let preview = run(&layout, &source, Scope::Project, true, false)
            .unwrap()
            .join("\n");
        assert!(preview.contains("Would import"), "{preview}");
        let installed = layout.skills_dirs()[0].join(name);
        assert!(!installed.exists());
        let imported = run(&layout, &source, Scope::Project, false, false)
            .unwrap()
            .join("\n");
        assert!(imported.contains("Imported"), "{imported}");
        assert_eq!(fs::read(installed.join(SKILL_FILE)).unwrap(), original);
        // Sharing a skill is copying its authored directory, not a learned-artifact pack.
        let reimported = run(&second, &installed, Scope::Project, false, false)
            .unwrap()
            .join("\n");
        assert!(reimported.contains("Imported"), "{reimported}");
        assert_eq!(
            fs::read(second.skills_dirs()[0].join(name).join(SKILL_FILE)).unwrap(),
            original
        );
        let edited = format!(
            "{}\nLocal review note.\n",
            String::from_utf8(original).unwrap()
        );
        fs::write(installed.join(SKILL_FILE), &edited).unwrap();
        let update = run(&layout, &source, Scope::Project, false, true)
            .unwrap()
            .join("\n");
        assert!(update.contains("local changes"), "{update}");
        assert_eq!(
            fs::read_to_string(installed.join(SKILL_FILE)).unwrap(),
            edited
        );
    }
    let catalog = discover(&second);
    for name in ["review", "pack-review"] {
        assert!(catalog.iter().any(|skill| skill.name == name));
    }
}
