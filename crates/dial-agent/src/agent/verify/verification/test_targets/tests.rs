use super::*;

fn entry(root: &Path, entry: &str) -> Result<FocusedCheck, String> {
    target_check(root, Path::new("python3"), &[], entry)
}

#[test]
fn entries_outside_the_workspace_or_with_unsafe_selectors_are_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("test_a.py"), "").unwrap();
    for bad in [
        "",
        "/etc/passwd",
        "../x.py",
        "test_a.py::",
        "test_a.py::x;rm",
        "missing.py",
    ] {
        assert!(entry(root.path(), bad).is_err(), "{bad:?}");
    }
    let outside = entry(root.path(), "/etc/passwd").unwrap_err();
    assert!(outside.contains("outside the workspace"), "{outside}");
    assert!(
        entry(root.path(), "notes.txt")
            .unwrap_err()
            .contains("does not exist")
    );
    std::fs::write(root.path().join("notes.txt"), "").unwrap();
    assert!(
        entry(root.path(), "notes.txt")
            .unwrap_err()
            .contains("no test runner")
    );
}

#[test]
fn absolute_entries_under_the_workspace_resolve_like_relative_ones() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("test_a.py"), "").unwrap();
    let relative = entry(root.path(), "test_a.py").unwrap();
    for absolute in [
        root.path().join("test_a.py"),
        root.path().canonicalize().unwrap().join("test_a.py"),
    ] {
        let check = entry(root.path(), absolute.to_str().unwrap()).unwrap();
        assert_eq!(check.command, relative.command, "{}", absolute.display());
    }
    assert!(
        entry(root.path(), root.path().join("../x.py").to_str().unwrap())
            .unwrap_err()
            .contains("..")
    );
}

#[test]
fn python_entries_pick_pytest_unittest_or_an_exit_status_script() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("tests")).unwrap();
    std::fs::write(root.path().join("tests/test_calc.py"), "").unwrap();
    std::fs::write(root.path().join("script.py"), "").unwrap();

    let unit = entry(root.path(), "tests/test_calc.py::TestCalc::test_div").unwrap();
    assert!(
        unit.command
            .ends_with("-m unittest 'tests.test_calc.TestCalc.test_div'")
            || unit
                .command
                .ends_with("-m unittest tests.test_calc.TestCalc.test_div"),
        "{}",
        unit.command
    );
    assert_eq!(unit.name, "verify: tests/test_calc.py::TestCalc::test_div");
    assert!(behavioral_target(&unit.name) && !exit_status_target(&unit.name));

    let script = entry(root.path(), "script.py").unwrap();
    assert!(!script.command.contains("unittest"), "{}", script.command);
    assert!(exit_status_target(&script.name) && !behavioral_target(&script.name));

    std::fs::write(root.path().join("pytest.ini"), "[pytest]\n").unwrap();
    let pytest = entry(root.path(), "tests/test_calc.py::test_div").unwrap();
    assert!(
        pytest.command.contains("-m pytest -q")
            && pytest.command.contains("tests/test_calc.py::test_div"),
        "{}",
        pytest.command
    );
    assert!(!behavioral_target("unit") && !exit_status_target("repro: bug"));
}

#[test]
fn go_entries_run_the_package_with_an_anchored_filter() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("go.mod"), "module x\n").unwrap();
    std::fs::create_dir(root.path().join("calc")).unwrap();
    std::fs::write(root.path().join("calc/calc_test.go"), "").unwrap();
    let check = entry(root.path(), "calc/calc_test.go::TestDiv").unwrap();
    assert_eq!(check.command, "go test -count=1 './calc' -run '^TestDiv$'");
}

#[test]
fn counts_come_from_each_runner_summary() {
    let cases = [
        (
            "test result: ok. 3 passed; 0 failed; 0 ignored\ntest result: FAILED. 1 passed; 2 failed;",
            (4, 2),
        ),
        ("==== 1 failed, 4 passed in 0.05s ====", (4, 1)),
        ("2 passed, 1 error in 0.10s", (2, 1)),
        (
            "Ran 5 tests in 0.001s\n\nFAILED (failures=1, errors=1)",
            (3, 2),
        ),
        ("Ran 3 tests in 0.001s\n\nOK (skipped=1)", (2, 0)),
        ("Tests:       1 failed, 6 passed, 7 total", (6, 1)),
        ("      Tests  2 passed (2)", (2, 0)),
    ];
    for (output, (passed, failed)) in cases {
        assert_eq!(
            test_counts(output),
            Some(TestCounts { passed, failed }),
            "{output}"
        );
    }
    assert_eq!(test_counts("all good"), None);
}

#[test]
fn javascript_entries_use_literal_jest_paths_and_nested_go_entries_use_their_module() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("node_modules/.bin")).unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{"scripts":{"test":"jest"}}"#,
    )
    .unwrap();
    let executable = if cfg!(windows) { "jest.cmd" } else { "jest" };
    std::fs::write(root.path().join("node_modules/.bin").join(executable), "").unwrap();
    std::fs::write(root.path().join("calc.test.ts"), "").unwrap();
    let js = entry(root.path(), "calc.test.ts::divides").unwrap();
    assert!(
        js.command
            .ends_with("--runInBand --runTestsByPath 'calc.test.ts' -t 'divides'"),
        "{}",
        js.command
    );
    std::fs::create_dir(root.path().join("service")).unwrap();
    std::fs::write(
        root.path().join("service/go.mod"),
        "module example.com/service\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("service/calc_test.go"),
        "package service\n",
    )
    .unwrap();
    assert_eq!(
        entry(root.path(), "service/calc_test.go::TestDiv")
            .unwrap()
            .command,
        "cd 'service' && go test -count=1 '.' -run '^TestDiv$'"
    );
}

#[test]
fn project_checks_resolve_cargo_node_go_and_python_targets() {
    let root = tempfile::tempdir().unwrap();
    // Cargo subproject
    let rust = root.path().join("engine");
    std::fs::create_dir(&rust).unwrap();
    std::fs::write(
        rust.join("Cargo.toml"),
        "[package]\nname = \"e\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let clippy = entry(root.path(), "engine:clippy").unwrap();
    assert_eq!(
        clippy.command,
        "cd 'engine' && cargo clippy --offline --all-targets -- -D warnings"
    );
    assert!(!behavioral_target(&clippy.name));
    let fmt = entry(root.path(), "engine:fmt").unwrap();
    assert_eq!(fmt.command, "cd 'engine' && cargo fmt --all -- --check");
    assert!(!behavioral_target(&fmt.name));
    let test = entry(root.path(), "engine:test").unwrap();
    assert_eq!(test.command, "cd 'engine' && cargo test --offline");
    assert!(behavioral_target(&test.name));
    let filtered = entry(root.path(), "engine:test::smoke").unwrap();
    assert_eq!(
        filtered.command,
        "cd 'engine' && cargo test --offline 'smoke'"
    );
    assert!(behavioral_target(&filtered.name));
    assert!(entry(root.path(), "engine:clippy::smoke").is_err());
    // Node subproject with scripts + pnpm lockfile
    let web = root.path().join("web");
    std::fs::create_dir(&web).unwrap();
    std::fs::write(
        web.join("package.json"),
        r#"{"scripts":{"lint":"eslint .","typecheck":"nuxt typecheck","build":"nuxt build","test":"node --test"}}"#,
    )
    .unwrap();
    std::fs::write(web.join("pnpm-lock.yaml"), "").unwrap();
    assert_eq!(
        entry(root.path(), "web:lint").unwrap().command,
        "cd 'web' && pnpm run lint"
    );
    assert_eq!(
        entry(root.path(), "web:typecheck").unwrap().command,
        "cd 'web' && pnpm run typecheck"
    );
    assert_eq!(
        entry(root.path(), "web:build").unwrap().command,
        "cd 'web' && pnpm run build"
    );
    assert!(behavioral_target(
        &entry(root.path(), "web:test").unwrap().name
    ));
    assert!(!behavioral_target(
        &entry(root.path(), "web:typecheck").unwrap().name
    ));

    // Python fmt (ruff) + typecheck (pyright) + test selector
    let py = root.path().join("py");
    std::fs::create_dir(&py).unwrap();
    std::fs::write(
        py.join("pyproject.toml"),
        "[tool.pytest.ini_options]\n[tool.ruff]\n[tool.pyright]\n",
    )
    .unwrap();
    assert_eq!(
        entry(root.path(), "py:fmt").unwrap().command,
        "cd 'py' && ruff format --check ."
    );
    assert_eq!(
        entry(root.path(), "py:typecheck").unwrap().command,
        "cd 'py' && pyright"
    );
    assert!(
        entry(root.path(), "py:test::smoke")
            .unwrap()
            .command
            .contains("-m pytest -q -k 'smoke'")
    );

    // Escape attempt is rejected.
    assert!(entry(root.path(), "../outside:clippy").is_err());
}

#[test]
fn directory_and_nested_ts_targets_resolve_without_falling_back_to_bash() {
    let root = tempfile::tempdir().unwrap();
    let tests_dir = root.path().join("web/tests/unit");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(tests_dir.join("a.test.mjs"), "").unwrap();
    std::fs::write(tests_dir.join("b.test.mjs"), "").unwrap();
    // Recursive directory discovery finds tests nested under web/tests/unit.
    let dir_check = entry(root.path(), "web/tests").unwrap();
    assert_eq!(
        dir_check.command,
        "node --test 'web/tests/unit/a.test.mjs' 'web/tests/unit/b.test.mjs'"
    );
    assert!(behavioral_target(&dir_check.name));
    // Nested package.json jest runner is found even when workspace root has no package.json.
    let app = root.path().join("app");
    std::fs::create_dir_all(app.join("node_modules/.bin")).unwrap();
    std::fs::write(app.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
    let executable = if cfg!(windows) { "jest.cmd" } else { "jest" };
    std::fs::write(app.join("node_modules/.bin").join(executable), "").unwrap();
    std::fs::write(app.join("calc.test.ts"), "").unwrap();
    let nested = entry(root.path(), "app/calc.test.ts::adds").unwrap();
    assert!(
        nested.command.starts_with("cd 'app' && ")
            && nested
                .command
                .contains("--runTestsByPath 'calc.test.ts' -t 'adds'"),
        "{}",
        nested.command
    );

    // Plain .ts test without jest/vitest uses `node --test`.
    std::fs::write(root.path().join("standalone.test.ts"), "").unwrap();
    let ts = entry(root.path(), "standalone.test.ts").unwrap();
    assert_eq!(ts.command, "node --test 'standalone.test.ts'");
}
