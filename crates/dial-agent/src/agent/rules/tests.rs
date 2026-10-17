use super::*;

fn write(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
fn layout(project: &Path, user: &Path) -> Layout {
    Layout::with_user_dir(project, user.to_path_buf()).unwrap()
}
fn sources(catalog: &Catalog) -> Vec<&str> {
    catalog
        .rules
        .iter()
        .map(|rule| rule.source.as_str())
        .collect()
}

#[test]
fn edit_paths_include_native_sloppy_targets_and_structured_renames() {
    let root = Path::new("/project");
    let sloppy = serde_json::json!({"input": "*** Edit File: \"source file.rs\" all\n*** Find\nold\n*** Replace\nnew"});
    assert_eq!(
        argument_paths(root, "edit", &sloppy),
        BTreeSet::from(["source file.rs".into()])
    );
    let patch =
        serde_json::json!({"files": [{"path":"old.rs", "rename":"new.rs", "diff":"-old\n+new"}]});
    assert_eq!(
        argument_paths(root, "edit", &patch),
        BTreeSet::from(["old.rs".into(), "new.rs".into()])
    );
}

#[test]
fn discovers_windsurf_rules() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        home.join(".codeium/windsurf/memories/global_rules.md"),
        "---\nalwaysApply: true\n---\nuser windsurf",
    );
    write(
        project.join(".windsurf/rules/ts.md"),
        "---\ntrigger: glob\nglobs: \"*.ts\"\n---\nts rule",
    );
    write(
        project.join(".windsurf/rules/nested/deep.md"),
        "---\nglobs: \"*.rs\"\n---\nnot top level",
    );
    write(
        project.join(".windsurf/rules/manual.md"),
        "---\ntrigger: manual\ndescription: manual\n---\nmanual",
    );
    let layout = layout(&project, &base.path().join("dial-user"));

    // Foreign user providers are opt-in: the home-level rule is absent by default.
    let off = Catalog::discover_with(&layout, &project, Some(&home), &[]);
    assert_eq!(sources(&off), ["project:windsurf:.windsurf/rules/ts.md"]);

    let on = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"]);
    assert_eq!(
        sources(&on),
        [
            "user:windsurf:.codeium/windsurf/memories/global_rules.md",
            "project:windsurf:.windsurf/rules/ts.md",
        ]
    );
    // A `manual` trigger has no globs or alwaysApply, so Dial refuses it as before.
    assert!(on.warnings.iter().any(|w| w.contains("manual.md")));
}

#[test]
fn discovers_cline_rules() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let repo = base.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let project = repo.join("pkg");
    fs::create_dir_all(&project).unwrap();
    write(
        repo.join(".clinerules/style.md"),
        "---\nglobs: \"*.rs\"\n---\nstyle",
    );
    write(repo.join(".clinerules/notes.txt"), "ignored");
    let layout = layout(&project, &base.path().join("dial-user"));

    // The walk reaches the git root, where the `.clinerules` directory lives.
    let dir = Catalog::discover_with(&layout, &project, Some(&home), &[]);
    assert_eq!(sources(&dir), ["project:cline:.clinerules/style.md"]);

    // The nearest `.clinerules` file wins over the directory above it.
    write(
        project.join(".clinerules"),
        "---\nglobs: \"*.ts\"\n---\nnearest",
    );
    let file = Catalog::discover_with(&layout, &project, Some(&home), &[]);
    assert_eq!(sources(&file), ["project:cline:.clinerules"]);
    assert_eq!(file.rules[0].body.trim(), "nearest");
}

#[test]
fn portable_rules_precedence() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        home.join(".codeium/windsurf/memories/global_rules.md"),
        "---\nalwaysApply: true\n---\nuser",
    );
    write(
        project.join(".windsurf/rules/global_rules.md"),
        "---\nalwaysApply: true\n---\nproject",
    );
    let layout = layout(&project, &base.path().join("dial-user"));

    // Same provider and name: the project file shadows the user file.
    let got = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"]);
    assert_eq!(
        sources(&got),
        ["project:windsurf:.windsurf/rules/global_rules.md"]
    );
    assert!(got.rules[0].body.contains("project"));
    assert!(!got.rules[0].body.contains("user"));
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_windsurf_rule_outside_root() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        base.path().join("secret.md"),
        "---\nalwaysApply: true\n---\nsecret",
    );
    fs::create_dir_all(project.join(".windsurf/rules")).unwrap();
    std::os::unix::fs::symlink(
        base.path().join("secret.md"),
        project.join(".windsurf/rules/leak.md"),
    )
    .unwrap();
    let layout = layout(&project, &base.path().join("dial-user"));

    let got = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"]);
    assert!(got.rules.is_empty());
    assert!(got.warnings.iter().any(|w| w.contains("leak.md")));
}
