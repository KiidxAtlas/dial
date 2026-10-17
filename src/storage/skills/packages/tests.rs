use super::*;

fn layout(root: &Path) -> Layout {
    Layout::with_user_dir(root, root.join("user")).unwrap()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dial-packages-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(url: &str, skills: &[&str]) -> LockEntry {
    LockEntry {
        url: url.into(),
        reference: Some("main".into()),
        path: Some("skills".into()),
        commit: "a".repeat(40),
        integrity: "b".repeat(64),
        skills: skills.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn lock_round_trips_and_keeps_optional_fields_out_when_absent() {
    let root = scratch("lock");
    let path = root.join(LOCK_FILE);
    let mut lock = LockFile {
        version: LOCK_VERSION,
        packages: BTreeMap::new(),
    };
    lock.packages
        .insert("pdf".into(), entry("https://x/r.git", &["pdf"]));
    lock.packages.insert(
        "bare".into(),
        LockEntry {
            reference: None,
            path: None,
            ..entry("https://x/b.git", &["bare"])
        },
    );
    write_lock(&path, &lock).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"ref\": \"main\""));
    assert!(!text.contains("\"bare\"\n") || !text.contains("\"ref\": null"));
    let back = read_lock(&path).unwrap();
    assert!(back.packages["pdf"] == lock.packages["pdf"]);
    assert!(back.packages["bare"] == lock.packages["bare"]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_lock_reads_as_empty_and_bad_versions_are_rejected() {
    let root = scratch("lockerr");
    let path = root.join(LOCK_FILE);
    assert!(read_lock(&path).unwrap().packages.is_empty());
    fs::write(&path, r#"{"version":9,"packages":{}}"#).unwrap();
    assert!(read_lock(&path).is_err());
    fs::write(&path, r#"{"version":1,"packages":{},"extra":true}"#).unwrap();
    assert!(read_lock(&path).is_err());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plan_uses_registry_only_when_configured_and_rejects_bare_words() {
    let root = scratch("plan");
    let index = root.join("index.json");
    fs::write(
        &index,
        r#"{"version":1,"skills":[{"name":"pdf","description":"PDF","url":"https://x/skills.git@v1#pdf"}]}"#,
    )
    .unwrap();
    let mut registries = BTreeMap::new();
    registries.insert("team".to_owned(), format!("file://{}", index.display()));

    let planned = plan("team/pdf@v2", &registries, &root).unwrap();
    assert_eq!(planned.name, "pdf");
    assert_eq!(planned.source.reference.as_deref(), Some("v2"));
    assert_eq!(planned.source.path.as_deref(), Some("pdf"));
    assert_eq!(planned.label, "team/pdf");

    let unknown = plan("team/missing", &registries, &root)
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("no skill"), "{unknown}");

    let none = BTreeMap::new();
    assert!(plan("pdf", &none, &root).is_err());
    assert!(plan("team/pdf", &none, &root).is_err());

    let git = plan("https://example.com/org/tools.git@main#pdf", &none, &root).unwrap();
    assert_eq!(git.name, "pdf");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn describe_change_reports_skills_and_files() {
    let old: BTreeMap<String, String> = [("a/SKILL.md", "1"), ("a/x", "1")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let new: BTreeMap<String, String> = [("a/SKILL.md", "2"), ("b/SKILL.md", "1")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let text = describe_change(&old, &new, &["a".to_owned()], &["b".to_owned()]);
    assert!(text.contains("new skills: b"), "{text}");
    assert!(text.contains("removed skills: a"), "{text}");
    assert!(
        text.contains("files: 1 added, 1 changed, 1 removed"),
        "{text}"
    );
    assert_eq!(
        describe_change(&old, &old, &["a".to_owned()], &["a".to_owned()]),
        "no file changes"
    );
}

#[test]
fn prune_keeps_live_entries_and_staging() {
    let root = scratch("prune");
    let layout = layout(&root);
    let store = store_root(&layout);
    fs::create_dir_all(store.join("live")).unwrap();
    fs::create_dir_all(store.join("dead")).unwrap();
    fs::create_dir_all(store.join(".staging-1")).unwrap();
    let mut lock = LockFile {
        version: LOCK_VERSION,
        packages: BTreeMap::new(),
    };
    lock.packages.insert(
        "pdf".into(),
        LockEntry {
            integrity: "live".into(),
            ..entry("https://x/r.git", &["pdf"])
        },
    );
    write_lock(&lock_path(&layout, Scope::User), &lock).unwrap();
    prune_store(&layout).unwrap();
    assert!(store.join("live").is_dir());
    assert!(!store.join("dead").exists());
    assert!(store.join(".staging-1").is_dir());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn removal_in_one_project_keeps_content_another_project_references() {
    let root = scratch("shared");
    let user = root.join("user");
    let project_a = root.join("a");
    let project_b = root.join("b");
    fs::create_dir_all(&project_a).unwrap();
    fs::create_dir_all(&project_b).unwrap();
    let layout_a = Layout::with_user_dir(&project_a, user.clone()).unwrap();
    let layout_b = Layout::with_user_dir(&project_b, user).unwrap();
    let store = store_root(&layout_a);
    let live_a = "1".repeat(64);
    let live_b = "2".repeat(64);
    let dead = "3".repeat(64);
    for name in [&live_a, &live_b, &dead] {
        fs::create_dir_all(store.join(name)).unwrap();
    }
    fs::create_dir_all(store.join(".staging-1")).unwrap();
    let with = |integrity: &str| LockFile {
        version: LOCK_VERSION,
        packages: [(
            "pdf".to_owned(),
            LockEntry {
                integrity: integrity.to_owned(),
                ..entry("https://x/r.git", &["pdf"])
            },
        )]
        .into_iter()
        .collect(),
    };
    save_lock(&layout_a, Scope::Project, &with(&live_a)).unwrap();
    save_lock(&layout_b, Scope::Project, &with(&live_b)).unwrap();
    // B removes its package; A's content must survive and B's own content is unreferenced now.
    save_lock(
        &layout_b,
        Scope::Project,
        &LockFile {
            version: LOCK_VERSION,
            packages: BTreeMap::new(),
        },
    )
    .unwrap();
    prune_store(&layout_b).unwrap();
    assert!(store.join(&live_a).is_dir());
    assert!(!store.join(&live_b).exists());
    assert!(!store.join(&dead).exists());
    assert!(store.join(".staging-1").is_dir());
    assert_eq!(roots(&layout_a).len(), 1);
    let _ = fs::remove_dir_all(&root);
}

#[test]
#[cfg(unix)]
fn lock_integrity_cannot_expose_external_or_malformed_roots() {
    let root = scratch("roots");
    let layout = layout(&root);
    let store = store_root(&layout);
    let good = "4".repeat(64);
    let link = "5".repeat(64);
    let outside = root.join("outside");
    fs::create_dir_all(store.join(&good)).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, store.join(&link)).unwrap();
    let mut lock = LockFile {
        version: LOCK_VERSION,
        packages: BTreeMap::new(),
    };
    for (name, integrity) in [
        ("good", good.clone()),
        ("link", link.clone()),
        ("absolute", outside.display().to_string()),
        ("traversal", "../../outside".to_owned()),
        ("uppercase", "A".repeat(64)),
    ] {
        lock.packages.insert(
            name.to_owned(),
            LockEntry {
                integrity,
                ..entry("https://x/r.git", &["pdf"])
            },
        );
    }
    write_lock(&lock_path(&layout, Scope::Project), &lock).unwrap();
    let found = roots(&layout);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, store.join(&good));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn replaced_entry_is_detected_against_snapshot() {
    let package = Package {
        name: "pdf".into(),
        scope: Scope::Project,
        source: Source {
            url: "https://x/r.git".into(),
            reference: Some("main".into()),
            path: Some("skills".into()),
        },
        commit: "a".repeat(40),
        integrity: "b".repeat(64),
        skills: vec!["pdf".into()],
    };
    assert!(same_entry(&entry("https://x/r.git", &["pdf"]), &package));
    let replaced = LockEntry {
        url: "https://y/other.git".into(),
        ..entry("https://x/r.git", &["pdf"])
    };
    assert!(!same_entry(&replaced, &package));
}

#[test]
fn install_update_and_remove_a_local_repository_end_to_end() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let root = scratch("e2e");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let layout = layout(&project);
    let remote = root.join("remote");
    fs::create_dir_all(remote.join("pdf")).unwrap();
    let git = |args: &[&str], dir: &Path| {
        let ok = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    let write_skill = |body: &str| {
        fs::write(
            remote.join("pdf/SKILL.md"),
            format!("---\nname: pdf\ndescription: PDF tools\n---\n{body}\n"),
        )
        .unwrap();
    };
    git(&["init", "--quiet"], &remote);
    write_skill("v1");
    git(&["add", "."], &remote);
    git(&["commit", "--quiet", "-m", "one"], &remote);
    let registries = BTreeMap::new();
    let cancel = AtomicBool::new(false);
    let spec = format!("{}#pdf", remote.display());

    let installed = install(
        &layout,
        &registries,
        Scope::Project,
        &spec,
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert!(installed.contains("Skills: pdf"), "{installed}");
    let duplicate = install(
        &layout,
        &registries,
        Scope::Project,
        &spec,
        &cancel,
        &mut |_| {},
    );
    assert!(
        duplicate
            .unwrap_err()
            .to_string()
            .contains("already installed")
    );
    let packages = list_packages(&layout).unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].skills, vec!["pdf".to_owned()]);
    assert!(
        roots(&layout)
            .iter()
            .all(|(_, root)| root.join("pdf/SKILL.md").is_file())
    );

    let up_to_date = update(&layout, Some("pdf"), &cancel, &mut |_| {}).unwrap();
    assert!(up_to_date.contains("already up to date"), "{up_to_date}");

    write_skill("v2");
    git(&["commit", "--quiet", "-am", "two"], &remote);
    let updated = update(&layout, Some("pdf"), &cancel, &mut |_| {}).unwrap();
    assert!(
        updated.contains("files: 0 added, 1 changed, 0 removed"),
        "{updated}"
    );
    assert_eq!(list_packages(&layout).unwrap()[0].commit.len(), 40);
    let body = fs::read_to_string(roots(&layout)[0].1.join("pdf/SKILL.md")).unwrap();
    assert!(body.contains("v2"));

    let removed = remove(&layout, "pdf").unwrap();
    assert!(removed.contains("Removed pdf"), "{removed}");
    assert!(list_packages(&layout).unwrap().is_empty());
    assert!(remove(&layout, "pdf").is_err());
    let store = store_root(&layout);
    let leftover = fs::read_dir(&store).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftover, 0);
    let _ = fs::remove_dir_all(&root);
}
