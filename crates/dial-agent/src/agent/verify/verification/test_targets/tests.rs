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
    assert_eq!(check.command, "go test './calc' -run '^TestDiv$'");
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
