use super::*;
use std::fs;
#[test]
fn python_checks_share_project_runner_without_sync_or_fallback() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("tests/test_app.py"), "").unwrap();
    std::fs::write(root.path().join("pytest.ini"), "").unwrap();
    std::fs::write(root.path().join("uv.lock"), "version = 1").unwrap();
    std::fs::write(root.path().join("app.py"), "").unwrap();
    let static_check = plan(root.path(), Path::new("wrong-python"), &["app.py".into()]).remove(0);
    let test_check =
        related_tests(root.path(), Path::new("wrong-python"), &["app.py".into()]).remove(0);
    for check in [&static_check, &test_check] {
        assert!(
            check
                .command
                .starts_with("uv run --offline --no-sync python")
        );
        assert!(!check.command.contains("wrong-python"));
    }
    assert!(environment_problem(&test_check, "No module named pytest").is_some());
    assert!(environment_problem(&test_check, "AssertionError: wrong answer").is_none());
    assert!(environment_problem(&static_check, "SyntaxError: invalid syntax").is_none());
    std::fs::remove_file(root.path().join("uv.lock")).unwrap();
    std::fs::write(root.path().join("poetry.lock"), "").unwrap();
    assert_eq!(
        python_runner(root.path(), Path::new("fallback")),
        "poetry run python"
    );
    let venv = venv_python(root.path());
    std::fs::create_dir_all(venv.parent().unwrap()).unwrap();
    std::fs::write(&venv, "").unwrap();
    assert!(
        python_runner(root.path(), Path::new("fallback"))
            .contains(&venv.to_string_lossy().to_string())
    );
}

#[test]
fn automatic_inputs_require_the_exact_isolated_builtin_and_a_direct_runner() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("app.py"), "value = 1\n").unwrap();
    let python = Path::new("/runtime/bin/python3.14");
    let mut check = plan(root.path(), python, &["app.py".into()]).remove(0);
    assert_eq!(
        automatic_python_source(root.path(), python, &[], &check),
        Some("app.py".into())
    );
    assert!(automatic_python_source(root.path(), python, &["wrapper".into()], &check).is_none());
    check.command.push_str("; true");
    assert!(automatic_python_source(root.path(), python, &[], &check).is_none());
    fs::write(root.path().join("uv.lock"), "version = 1").unwrap();
    let wrapped = plan(root.path(), python, &["app.py".into()]).remove(0);
    assert!(automatic_python_source(root.path(), python, &[], &wrapped).is_none());
    let cargo = FocusedCheck {
        name: "cargo check".into(),
        command: "cargo check".into(),
        timeout_secs: 0,
    };
    assert!(automatic_python_source(root.path(), python, &[], &cargo).is_none());
}

#[test]
fn project_venv_executes_ast_and_tests_instead_of_invalid_fallback() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let created = std::process::Command::new("python")
        .args(["-m", "venv", "--without-pip"])
        .arg(root.path().join(".venv"))
        .status()
        .unwrap();
    #[cfg(not(windows))]
    let created = std::process::Command::new("python3")
        .args(["-m", "venv", "--without-pip"])
        .arg(root.path().join(".venv"))
        .status()
        .unwrap();
    assert!(created.success());
    std::fs::create_dir(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("app.py"), "answer = 42\n").unwrap();
    std::fs::write(root.path().join("tests/test_app.py"), "import unittest, sys\nclass Test(unittest.TestCase):\n def test_project_env(self):\n  self.assertNotEqual(sys.prefix, sys.base_prefix)\n").unwrap();
    let mut checks = plan(
        root.path(),
        Path::new("missing-interpreter"),
        &["app.py".into()],
    );
    checks.extend(related_tests(
        root.path(),
        Path::new("missing-interpreter"),
        &["app.py".into()],
    ));
    assert_eq!(checks.len(), 2);
    for check in checks {
        #[cfg(windows)]
        let output = {
            let command = format!(
                "{}; if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}",
                check.command
            );
            std::process::Command::new(
                std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            )
            .args(["-NoProfile", "-NonInteractive", "-Command", &command])
            .current_dir(root.path())
            .output()
            .unwrap()
        };
        #[cfg(not(windows))]
        let output = std::process::Command::new("sh")
            .args(["-c", &check.command])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            check.command,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn related_tests_are_direct_bounded_and_quoted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("tests/test_math.py"), "").unwrap();
    let checks = related_tests(
        root.path(),
        Path::new("/a path/python"),
        &[
            "math.py".into(),
            "math.py".into(),
            "other.py".into(),
            "bad;cmd.py".into(),
        ],
    );
    assert_eq!(checks.len(), 1);
    #[cfg(windows)]
    assert!(checks[0].command.starts_with("& '/a path/python'"));
    #[cfg(not(windows))]
    assert!(checks[0].command.starts_with("'/a path/python'"));
    assert!(checks[0].command.contains("test_math.py"));
}

#[test]
fn related_checks_follow_project_metadata_and_source_layout() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    for dir in ["src/agent", "tests", "app", "src", "node_modules/.bin"] {
        std::fs::create_dir_all(path.join(dir)).unwrap();
    }
    std::fs::write(path.join("Cargo.toml"), "[package]\nname='fixture'\n").unwrap();
    std::fs::write(path.join("src/lib.rs"), "").unwrap();
    std::fs::write(
        path.join("src/agent/parser.rs"),
        "#[cfg(test)] mod tests { #[test] fn works() {} }\n",
    )
    .unwrap();
    std::fs::write(path.join("pyproject.toml"), "[tool.pytest.ini_options]\n").unwrap();
    std::fs::write(
        path.join("tests/test_config.py"),
        "def test_config(): pass\n",
    )
    .unwrap();
    std::fs::write(path.join("go.mod"), "module fixture\n").unwrap();
    std::fs::write(path.join("app/main.go"), "package app\n").unwrap();
    std::fs::write(path.join("app/main_test.go"), "package app\n").unwrap();
    std::fs::write(
        path.join("package.json"),
        r#"{"scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(path.join("src/ui.ts"), "export const ui = true;\n").unwrap();
    std::fs::write(
        path.join("src/ui.test.ts"),
        "import { test } from 'vitest';\n",
    )
    .unwrap();
    let vitest = path.join(if cfg!(windows) {
        "node_modules/.bin/vitest.cmd"
    } else {
        "node_modules/.bin/vitest"
    });
    std::fs::write(&vitest, "").unwrap();

    let checks = related_tests(
        path,
        Path::new("python3"),
        &[
            "src/agent/parser.rs".into(),
            "config.py".into(),
            "app/main.go".into(),
            "src/ui.ts".into(),
        ],
    );
    assert_eq!(checks.len(), 4);
    assert!(checks[0].command.contains("'agent::parser::tests::'"));
    assert!(
        checks[1]
            .command
            .contains("-m pytest -q 'tests/test_config.py'")
    );
    assert_eq!(checks[2].command, "go test './app'");
    assert!(
        checks[3].command.contains("vitest")
            && checks[3].command.contains("run")
            && checks[3].command.contains("src/ui.test.ts")
    );
}

#[test]
fn related_metadata_is_shared_within_selection_but_refreshed_between_calls() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    for dir in ["app", "other", "src", "node_modules/.bin"] {
        fs::create_dir_all(path.join(dir)).unwrap();
    }
    fs::write(path.join("go.mod"), "module fixture\n").unwrap();
    fs::write(path.join("app/main_test.go"), "package app\n").unwrap();
    for name in ["first", "second"] {
        fs::write(path.join(format!("src/{name}.test.ts")), "").unwrap();
    }
    for runner in ["vitest", "jest"] {
        let executable = if cfg!(windows) {
            format!("{runner}.cmd")
        } else {
            runner.into()
        };
        fs::write(path.join("node_modules/.bin").join(executable), "").unwrap();
    }
    let changed = [
        "src/second.ts".into(),
        "app/main.go".into(),
        "src/first.ts".into(),
        "app/other.go".into(),
        "src/second.ts".into(),
        "other/main.go".into(),
        "other/extra.go".into(),
    ];
    let select = || {
        let mut javascript_reads = 0;
        let mut go_scans = HashMap::new();
        let checks = related_tests_with_metadata(
            path,
            Path::new("python3"),
            &changed,
            &[],
            || {
                javascript_reads += 1;
                javascript_test_runner(path)
            },
            |directory| {
                *go_scans.entry(directory.to_owned()).or_insert(0) += 1;
                has_go_package_tests(path, directory)
            },
        );
        // Three JS paths resolve once, four Go paths scan each of two packages once.
        // The same bounds apply when metadata or a package's tests are absent.
        assert_eq!(javascript_reads, 1);
        assert_eq!(
            go_scans,
            HashMap::from([(PathBuf::from("app"), 1), (PathBuf::from("other"), 1)])
        );
        assert_eq!(checks, related_tests(path, Path::new("python3"), &changed));
        checks
    };
    // Missing and malformed metadata are not cached across invocations either.
    assert_eq!(select().len(), 1);
    fs::write(path.join("package.json"), "not json").unwrap();
    assert_eq!(select().len(), 1);
    fs::write(
        path.join("package.json"),
        r#"{"scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    let checks = select();
    assert_eq!(checks.len(), 3);
    assert_eq!(checks[0].name, "related vitest test: src/second.test.ts");
    assert_eq!(checks[1].command, "go test './app'");
    assert_eq!(checks[2].name, "related vitest test: src/first.test.ts");
    assert!(checks[0].command.contains(" run 'src/second.test.ts'"));

    fs::write(path.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
    fs::remove_file(path.join("app/main_test.go")).unwrap();
    fs::write(path.join("other/new_test.go"), "package other\n").unwrap();
    let checks = select();
    assert_eq!(checks.len(), 3);
    assert_eq!(checks[0].name, "related jest test: src/second.test.ts");
    assert_eq!(checks[1].name, "related jest test: src/first.test.ts");
    assert_eq!(checks[2].command, "go test './other'");
    assert!(
        checks[0]
            .command
            .contains(" --runInBand 'src/second.test.ts'")
    );

    let jest = if cfg!(windows) { "jest.cmd" } else { "jest" };
    fs::remove_file(path.join("node_modules/.bin").join(jest)).unwrap();
    assert_eq!(select().len(), 1);
    assert!(
        related_tests_with_metadata(
            path,
            Path::new("python3"),
            &["README.md".into()],
            &[],
            || panic!("unrelated paths must not load JavaScript metadata"),
            |_| panic!("unrelated paths must not scan Go packages"),
        )
        .is_empty()
    );
}

#[test]
fn nested_crates_without_a_root_manifest_are_checked_by_their_owner() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("tools/gen/src")).unwrap();
    std::fs::write(
        root.path().join("tools/gen/Cargo.toml"),
        "[package]\nname='gen'\n",
    )
    .unwrap();
    let checks = plan(
        root.path(),
        Path::new("python3"),
        &[
            "tools/gen/src/main.rs".into(),
            "tools/gen/src/lib.rs".into(),
            "loose.rs".into(),
        ],
    );
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].name, "cargo check: tools/gen");
    assert!(
        checks[0]
            .command
            .ends_with("--manifest-path 'tools/gen/Cargo.toml'")
    );
}

#[test]
fn coverage_limits_name_paths_beyond_the_related_test_cap() {
    let few = vec![PathBuf::from("a.rs"); RELATED_TEST_PATH_LIMIT];
    assert!(coverage_limits(&few).is_empty());
    let many = vec![PathBuf::from("a.rs"); RELATED_TEST_PATH_LIMIT + 3];
    let limits = coverage_limits(&many);
    assert_eq!(limits.len(), 1);
    assert!(limits[0].contains("first 32 of 35"), "{limits:?}");
}
#[test]
fn empty_suites_are_not_behavioral_evidence() {
    assert!(zero_tests("running 0 tests\n"));
    assert!(zero_tests("Ran 0 tests in 0.000s"));
    assert!(!zero_tests("running 4 tests\nrunning 0 tests\n"));
    assert!(zero_tests("? example/pkg [no test files]\n"));
    assert!(zero_tests("No tests found"));
    assert!(!zero_tests("build complete"));
    // A mixed `go test ./...` run passes when some package ran tests; all-testless stays empty.
    let mixed = "ok  \texample/tested\t0.012s\n?   \texample/helper\t[no test files]\n";
    assert!(!zero_tests(mixed));
    assert!(zero_tests(
        "?   \texample/helper\t[no test files]\n?   \texample/other\t[no test files]\n"
    ));
    assert!(zero_tests(
        "ok  \texample/tested\t0.01s [no tests to run]\n?   \texample/helper\t[no test files]\n"
    ));
}
