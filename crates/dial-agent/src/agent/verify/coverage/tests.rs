use super::*;
#[cfg(unix)]
use std::process::Command;

fn version(path: &str, before: Option<&str>, after: &str) -> TurnVersion {
    TurnVersion {
        path: path.into(),
        in_project: true,
        before: before.map(|text| text.as_bytes().to_vec()),
        after: Some(after.as_bytes().to_vec()),
    }
}

#[cfg(unix)]
fn run(root: &Path, probe: &Probe, command: &str) -> std::process::Output {
    let ws = Workspace::new(root).unwrap();
    Command::new("bash")
        .arg("-c")
        .arg(
            probe
                .wrap(&ws, command)
                .expect("the default shell resolves"),
        )
        .current_dir(root)
        .output()
        .unwrap()
}

#[test]
fn paths_compare_without_verbatim_prefixes_or_url_forms() {
    assert_eq!(
        interpreter_path(Path::new(r"\\?\C:\work\m.py")),
        r"C:\work\m.py"
    );
    assert_eq!(
        interpreter_path(Path::new(r"\\?\UNC\server\share\m.py")),
        r"\\server\share\m.py"
    );
    assert_eq!(interpreter_path(Path::new("/work/m.py")), "/work/m.py");
    assert_eq!(path_key("file:///work/a.js"), path_key("/work/a.js"));
    if cfg!(windows) {
        assert_eq!(path_key("file:///C:/Work/a.js"), path_key(r"c:\work\A.js"));
    }
}

#[cfg(unix)]
fn available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn changed_lines_skip_unchanged_blank_and_comment_lines() {
    let before = "a = 1\nb = 2\n";
    let after = "a = 1\n\n# note\nb = 3\nc = 4\n";
    assert_eq!(changed_lines(Some(before), after), BTreeSet::from([4, 5]));
    assert_eq!(changed_lines(None, "x\n  // y\nz"), BTreeSet::from([1, 3]));
    assert_eq!(ranges(&[3, 4, 5, 9, 11, 12]), "3-5, 9, 11-12");
    assert_eq!(percent_decode("file:///a%20b/c.js"), "file:///a b/c.js");
}

#[test]
fn unsupported_files_are_not_instrumented() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "fn f() {}\n").unwrap();
    assert!(prepare(root.path(), &[version("lib.rs", None, "fn f() {}\n")]).is_none());
}

#[cfg(unix)]
#[test]
fn python_collector_names_changed_lines_the_check_never_ran_and_cleans_up() {
    if !available("python3") {
        eprintln!("python3 unavailable; skipped");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = "def used(x):\n    return x + 1\n\n\ndef unused(x):\n    if x:\n        return 2\n    return 3\n";
    std::fs::write(root.path().join("mod.py"), source).unwrap();
    // A project sitecustomize must still run.
    std::fs::create_dir(root.path().join("site")).unwrap();
    std::fs::write(
        root.path().join("site/sitecustomize.py"),
        "import builtins\nbuiltins.PROJECT_HOOK = True\n",
    )
    .unwrap();
    let probe = prepare(root.path(), &[version("mod.py", None, source)]).unwrap();
    let scratch = root.path().join(crate::workspace_copy::SCRATCH_DIR);
    assert!(scratch.exists());
    let out = run(
        root.path(),
        &probe,
        "PYTHONPATH=\"$PYTHONPATH:site\" python3 -c 'import builtins, mod; assert builtins.PROJECT_HOOK; assert mod.used(1) == 2'",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = probe.report("unit").unwrap();
    assert!(
        line.contains("mod.py: changed lines 6-8 never executed"),
        "{line}"
    );
    drop(probe);
    assert!(
        !scratch.exists(),
        "the probe removes the scratch it created"
    );
}

#[cfg(unix)]
#[test]
fn python_files_the_check_never_imported_are_named() {
    if !available("python3") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("other.py"), "x = 1\n").unwrap();
    let probe = prepare(root.path(), &[version("other.py", None, "x = 1\n")]).unwrap();
    assert!(
        run(root.path(), &probe, "python3 -c 'pass'")
            .status
            .success()
    );
    assert!(
        probe
            .report("unit")
            .unwrap()
            .contains("other.py: never loaded by this check")
    );
}

#[cfg(unix)]
#[test]
fn node_block_coverage_names_unexecuted_branches() {
    if !available("node") {
        eprintln!("node unavailable; skipped");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let source = "function f(x) {\n  if (x > 0) {\n    return 'pos';\n  }\n  return 'neg';\n}\nmodule.exports = f;\n";
    std::fs::write(root.path().join("f.js"), source).unwrap();
    let probe = prepare(root.path(), &[version("f.js", None, source)]).unwrap();
    let out = run(
        root.path(),
        &probe,
        "node -e \"if (require('./f.js')(1) !== 'pos') process.exit(1)\"",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let line = probe.report("unit").unwrap();
    assert!(
        line.contains("f.js: changed line 5 never executed"),
        "{line}"
    );
}
