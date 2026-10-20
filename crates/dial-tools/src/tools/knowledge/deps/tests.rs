use super::*;
use std::path::Path;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn dev(ws: Workspace, deny_read: Vec<PathBuf>) -> Workspace {
    ws.with_policy(Policy::Dev {
        deny_read,
        extra_write: Vec::new(),
        allow_network: false,
    })
}

const LOCK: &str = r#"
version = 4

[[package]]
name = "demo-crate"
version = "1.2.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "demo-crate"
version = "0.9.0"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "local"
version = "0.1.0"
"#;

/// A project with a Cargo.lock and a cargo home holding only demo-crate 1.2.0.
fn cargo_fixture() -> (tempfile::TempDir, tempfile::TempDir, Caches) {
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("Cargo.lock"), LOCK);
    let home = tempfile::tempdir().unwrap();
    let crate_dir = home
        .path()
        .join("registry/src/index.crates.io-0123456789abcdef/demo-crate-1.2.0");
    write(
        &crate_dir.join("Cargo.toml"),
        "[package]\nname = \"demo-crate\"\n",
    );
    write(
        &crate_dir.join("src/lib.rs"),
        "pub fn needle() {}\npub fn other() {}\n",
    );
    let caches = Caches {
        cargo_home: Some(home.path().to_path_buf()),
        go_mod_cache: None,
    };
    (project, home, caches)
}

#[test]
fn cargo_resolves_the_locked_registry_source() {
    let (project, _home, caches) = cargo_fixture();
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(&ws, &json!({"name": "demo_crate"}), &caches).unwrap();
    assert!(
        out.starts_with("cargo demo-crate 1.2.0 (resolved from Cargo.lock, registry+"),
        "{out}"
    );
    assert!(out.contains("other locked versions: 0.9.0"), "{out}");
    assert!(out.contains("  src/\n"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "demo-crate", "query": "needle"}),
        &caches,
    )
    .unwrap();
    assert!(out.contains("src/lib.rs:1: pub fn needle() {}"), "{out}");
    assert!(!out.contains("other()"), "{out}");

    let out = run(
        &ws,
        &json!({"name": "demo-crate", "path": "src/lib.rs:2-2"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.contains("2:pub fn other() {}\n[src/lib.rs lines 2-2 of 2]"),
        "{out}"
    );

    // Locked but not downloaded: reported, never fetched.
    let error = run(&ws, &json!({"name": "demo-crate@0.9.0"}), &caches).unwrap_err();
    assert!(
        error.contains("is locked but its source is not in"),
        "{error}"
    );
    let error = run(
        &ws,
        &json!({"name": "local", "ecosystem": "cargo"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("path dependency"), "{error}");
    let error = run(&ws, &json!({"name": "absent"}), &caches).unwrap_err();
    assert!(error.contains("absent is not in Cargo.lock"), "{error}");
}

#[test]
fn package_paths_never_leave_the_package_root() {
    let (project, home, caches) = cargo_fixture();
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    for path in ["../../../../../etc/passwd", "/etc/passwd", "src/../../x"] {
        let error = run(&ws, &json!({"name": "demo-crate", "path": path}), &caches).unwrap_err();
        assert!(error.contains("without `..`"), "{path}: {error}");
    }
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("secret"), "token\n");
    let crate_dir = home
        .path()
        .join("registry/src/index.crates.io-0123456789abcdef/demo-crate-1.2.0");
    std::os::unix::fs::symlink(outside.path(), crate_dir.join("escape")).unwrap();
    let error = run(
        &ws,
        &json!({"name": "demo-crate", "path": "escape/secret"}),
        &caches,
    )
    .unwrap_err();
    assert!(error.contains("escapes the package root"), "{error}");
    // Searches do not follow the link either.
    let out = run(
        &ws,
        &json!({"name": "demo-crate", "query": "token"}),
        &caches,
    )
    .unwrap();
    assert!(out.contains("no matches"), "{out}");
}

#[test]
fn caches_outside_the_workspace_follow_the_sandbox_read_policy() {
    let (project, home, caches) = cargo_fixture();
    // Strict (also the default): the shell could not read the cache, so neither may this tool.
    let strict = Workspace::new(project.path()).unwrap();
    let error = run(&strict, &json!({"name": "demo-crate"}), &caches).unwrap_err();
    assert!(error.contains("/add-dir"), "{error}");
    // Dev, but the cache is under a denied path.
    let denied = dev(
        Workspace::new(project.path()).unwrap(),
        vec![home.path().to_path_buf()],
    );
    let error = run(&denied, &json!({"name": "demo-crate"}), &caches).unwrap_err();
    assert!(error.contains("denies reading"), "{error}");
    // An attached root grants the read even under the strict policy.
    let added = Workspace::new(project.path())
        .unwrap()
        .with_additional_roots(&[home.path().to_path_buf()])
        .unwrap();
    assert!(run(&added, &json!({"name": "demo-crate"}), &caches).is_ok());
}

#[test]
fn npm_resolves_installed_packages_inside_the_workspace() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    write(&root.join("package.json"), "{\"dependencies\": {}}");
    write(
        &root.join("node_modules/left-pad/package.json"),
        "{\"name\": \"left-pad\", \"version\": \"1.3.0\"}",
    );
    write(
        &root.join("node_modules/left-pad/index.js"),
        "module.exports = leftPad;\n",
    );
    write(
        &root.join("node_modules/@scope/pkg/package.json"),
        "{\"version\": \"2.0.0\"}",
    );
    // Inside the workspace, so the default strict policy reads it.
    let ws = Workspace::new(root).unwrap();
    let caches = Caches::default();
    let out = run(
        &ws,
        &json!({"name": "left-pad", "query": "leftPad"}),
        &caches,
    )
    .unwrap();
    assert!(
        out.starts_with("npm left-pad 1.3.0 (resolved from node_modules/left-pad/package.json)"),
        "{out}"
    );
    assert!(
        out.contains("index.js:1: module.exports = leftPad;"),
        "{out}"
    );
    let out = run(
        &ws,
        &json!({"name": "@scope/pkg", "ecosystem": "npm"}),
        &caches,
    )
    .unwrap();
    assert!(out.starts_with("npm @scope/pkg 2.0.0"), "{out}");
    let error = run(&ws, &json!({"name": "left-pad@9.9.9"}), &caches).unwrap_err();
    assert!(error.contains("node_modules has 1.3.0"), "{error}");

    for name in ["../outside", "a/../../b", "@scope/../x", ""] {
        let error = run(&ws, &json!({"name": name, "ecosystem": "npm"}), &caches).unwrap_err();
        assert!(error.contains("invalid package name"), "{name}: {error}");
    }
    // A linked package pointing out of node_modules is refused rather than followed.
    let outside = tempfile::tempdir().unwrap();
    write(
        &outside.path().join("package.json"),
        "{\"version\": \"0.0.1\"}",
    );
    std::os::unix::fs::symlink(outside.path(), root.join("node_modules/linked")).unwrap();
    let error = run(&ws, &json!({"name": "linked", "ecosystem": "npm"}), &caches).unwrap_err();
    assert!(error.contains("refusing to follow it"), "{error}");
}

#[test]
fn python_resolves_distributions_and_import_names_in_the_project_venv() {
    let project = tempfile::tempdir().unwrap();
    let site = project.path().join(".venv/lib/python3.12/site-packages");
    write(
        &site.join("PyYAML-6.0.1.dist-info/top_level.txt"),
        "_yaml\nyaml\n",
    );
    write(
        &site.join("yaml/__init__.py"),
        "def safe_load(stream): ...\n",
    );
    write(
        &project.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\n",
    );
    let ws = Workspace::new(project.path()).unwrap();
    let caches = Caches::default();
    for name in ["pyyaml", "yaml"] {
        let out = run(&ws, &json!({"name": name, "query": "safe_load"}), &caches).unwrap();
        assert!(out.starts_with("python PyYAML 6.0.1"), "{name}: {out}");
        assert!(out.contains("__init__.py:1: def safe_load"), "{out}");
    }
}

#[test]
fn go_resolves_required_modules_from_the_module_cache() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("go.mod"),
        "module example.com/app\n\ngo 1.22\n\nrequire (\n\tgithub.com/Foo/bar v1.2.3 // indirect\n)\n",
    );
    let cache = tempfile::tempdir().unwrap();
    write(
        &cache.path().join("github.com/!foo/bar@v1.2.3/bar.go"),
        "package bar\n",
    );
    let caches = Caches {
        cargo_home: None,
        go_mod_cache: Some(cache.path().to_path_buf()),
    };
    let ws = dev(Workspace::new(project.path()).unwrap(), Vec::new());
    let out = run(&ws, &json!({"name": "bar"}), &caches).unwrap();
    assert!(
        out.starts_with("go github.com/Foo/bar v1.2.3 (resolved from go.mod)"),
        "{out}"
    );
    assert!(out.contains("  bar.go\n"), "{out}");
}

#[test]
fn schema_is_read_only_and_needs_no_approval() {
    let tool = DepSource;
    assert!(!tool.needs_approval(&json!({"name": "x"})));
    assert!(tool.read_only_hint());
    assert_eq!(tool.concurrency(&json!({})), Concurrency::Shared);
    assert_eq!(tool.schema()["function"]["name"], "dep_source");
}
