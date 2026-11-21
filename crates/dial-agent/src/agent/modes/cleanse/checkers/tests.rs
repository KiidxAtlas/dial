use super::*;

#[test]
fn discovers_local_eslint_and_prefers_it_over_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // Discovery lists files through git, so the fixture is its own repository. An enclosing
    // checkout's ignore rules (this one ignores `/.tmp/`) would otherwise hide every file.
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let name = if cfg!(windows) {
        "eslint.cmd"
    } else {
        "eslint"
    };
    let local = root.join("node_modules/.bin").join(name);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, "#!/bin/sh\n").unwrap();
    crate::platform::fs::set_mode(&local, 0o755).unwrap();
    std::fs::write(root.join(".eslintrc.json"), "{}").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.js"), "let a = 1;\n").unwrap();

    let suite = discover(root, false);
    let plan = suite
        .plans()
        .iter()
        .find(|plan| plan.id == "eslint:.")
        .expect("eslint plan discovered");
    assert!(Path::new(&plan.argv[0]).ends_with(Path::new("node_modules/.bin").join(name)));
    assert_eq!(plan.display, "eslint . --format=json");
    assert_eq!(plan.parser, Kind::Eslint);
}

#[test]
fn shell_scripts_are_batched_under_shellcheck_when_present() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let files: Vec<String> = (0..SHELLCHECK_BATCH_SIZE + 1)
        .map(|index| format!("s{index}.sh"))
        .collect();
    let mut suite = Suite::new(root);
    // With no shellcheck binary the batches are skipped, never silently dropped.
    if resolve_binary(&["shellcheck"], root, root).is_none() {
        shell(&mut suite, root, &files);
        assert_eq!(suite.skipped().len(), 2);
        assert!(suite.plans().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn swiftlint_runs_at_configured_roots_and_is_optional_for_bare_packages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    for sub in [".", "App"] {
        let local = root.join(sub).join("vendor/bin/swiftlint");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, "#!/bin/sh\n").unwrap();
        crate::platform::fs::set_mode(&local, 0o755).unwrap();
    }
    let strings = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    // A `.swiftlint.yml` picks the lint roots; `swift test` follows `Package.swift`.
    let files = strings(&["Package.swift", "App/.swiftlint.yml", "App/Sources/A.swift"]);
    let mut suite = Suite::new(root);
    swift(&mut suite, root, &files, true);
    let ids: Vec<&str> = suite.plans().iter().map(|plan| plan.id.as_str()).collect();
    assert!(ids.contains(&"swiftlint:App"), "{ids:?}");
    assert!(!ids.contains(&"swiftlint:."), "{ids:?}");
    let lint = &suite.plans()[0];
    assert_eq!(lint.display, "swiftlint lint --reporter json --quiet");
    assert_eq!(lint.parser, Kind::Swiftlint);
    let tests_planned = ids.contains(&"swift-test:.")
        || suite
            .skipped()
            .iter()
            .any(|skip| skip.label == "swift test (.)");
    assert!(tests_planned);

    // Without configuration a package root is linted when SwiftLint is installed, else skipped quietly.
    let mut bare = Suite::new(root);
    swift(
        &mut bare,
        root,
        &strings(&["Package.swift", "Sources/B.swift"]),
        false,
    );
    assert_eq!(bare.plans()[0].id, "swiftlint:.");
    let empty = tempfile::tempdir().expect("tempdir");
    let mut missing = Suite::new(empty.path());
    swift(
        &mut missing,
        empty.path(),
        &strings(&["Package.swift", "B.swift"]),
        false,
    );
    if resolve_binary(&["swiftlint"], empty.path(), empty.path()).is_none() {
        assert!(missing.plans().is_empty() && missing.skipped().is_empty());
    }

    // No Swift files (a `Package.swift` manifest counts as one), no Swift checkers.
    let mut none = Suite::new(root);
    swift(
        &mut none,
        root,
        &strings(&[".swiftlint.yml", "README.md"]),
        true,
    );
    assert!(none.plans().is_empty() && none.skipped().is_empty());
}

#[test]
fn custom_unknown_parser_is_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let suite = custom(
        dir.path(),
        vec![CustomSpec {
            label: "rubo".into(),
            cwd: None,
            command: vec!["sh".into()],
            parser: Some("rubocop".into()),
        }],
    );
    assert!(suite.plans().is_empty());
    assert_eq!(suite.skipped()[0].reason, "unsupported parser: rubocop");
}

#[test]
fn topmost_roots_drops_nested_packages() {
    let roots = topmost_roots(vec!["a/b".into(), "a".into(), "c".into()]);
    assert_eq!(roots, vec!["a".to_owned(), "c".to_owned()]);
}
