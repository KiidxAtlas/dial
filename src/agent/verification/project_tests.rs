use super::*;

#[test]
fn nested_project_checks_execute_in_their_own_environment() {
    let root = tempfile::tempdir().unwrap();
    let service = root.path().join("services/my api");
    std::fs::create_dir_all(service.join("src")).unwrap();
    std::fs::create_dir(service.join("tests")).unwrap();
    std::fs::write(
        service.join("pyproject.toml"),
        "[project]\nname='api'\nversion='1.0'\n",
    )
    .unwrap();
    std::fs::write(service.join("src/app.py"), "answer = 42\n").unwrap();
    std::fs::write(service.join("tests/test_app.py"), "import unittest, sys, pathlib\nclass Test(unittest.TestCase):\n def test_owner(self):\n  self.assertNotEqual(sys.prefix, sys.base_prefix)\n  self.assertTrue(pathlib.Path('src/app.py').is_file())\n").unwrap();
    assert!(
        std::process::Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(service.join(".venv"))
            .status()
            .unwrap()
            .success()
    );
    let changed = [PathBuf::from("services/my api/src/app.py")];
    let mut checks = plan(root.path(), Path::new("/missing/root/python"), &changed);
    checks.extend(related_tests(
        root.path(),
        Path::new("/missing/root/python"),
        &changed,
    ));
    assert_eq!(checks.len(), 2);
    for check in checks {
        assert!(check.command.contains("services/my api"));
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
    let override_check = plan_with_runner(
        root.path(),
        Path::new("missing"),
        &changed,
        &["python3".into(), "-I".into()],
    )
    .remove(0);
    assert!(override_check.command.contains("'python3' '-I'"));
    assert!(!override_check.command.contains(".venv"));
    assert_eq!(
        python_owner(root.path(), Path::new("../escape.py")),
        root.path()
    );
}

#[test]
fn structured_outcomes_never_convert_setup_or_empty_checks_into_success() {
    let check = FocusedCheck {
        name: "related Python tests: app".into(),
        command: "python -m pytest".into(),
        timeout_secs: 1,
    };
    for (text, error, cancelled, expected) in [
        (
            "1 passed\n[exit code: 0]",
            false,
            false,
            CheckStatus::Passed,
        ),
        (
            "AssertionError\n[exit code: 1]",
            true,
            false,
            CheckStatus::CodeFailed,
        ),
        (
            "No module named pytest\n[exit code: 1]",
            true,
            false,
            CheckStatus::EnvironmentUnavailable,
        ),
        (
            "approval denied",
            true,
            false,
            CheckStatus::PermissionDenied,
        ),
        ("[timed out after 1s]", true, false, CheckStatus::TimedOut),
        ("", true, true, CheckStatus::Cancelled),
        (
            "no tests ran\n[exit code: 0]",
            false,
            false,
            CheckStatus::Unverified,
        ),
    ] {
        let output = crate::runtime::tool_output::ToolOutput {
            execution: Default::default(),
            text: text.into(),
            is_error: error,
            data: None,
        };
        let mut output = output.with_process(crate::runtime::execution::ProcessOutcome {
            started: !matches!(
                expected,
                CheckStatus::PermissionDenied | CheckStatus::Cancelled
            ),
            exit_code: match expected {
                CheckStatus::Passed | CheckStatus::Unverified => Some(0),
                CheckStatus::CodeFailed | CheckStatus::EnvironmentUnavailable => Some(1),
                _ => None,
            },
            signal: None,
            timed_out: expected == CheckStatus::TimedOut,
            cancelled,
            output_complete: !matches!(expected, CheckStatus::TimedOut | CheckStatus::Cancelled),
        });
        if expected == CheckStatus::PermissionDenied {
            output = output.with_status(crate::tools::ToolStatus::Blocked);
        }
        assert_eq!(classify(&check, &output, cancelled), expected, "{text}");
    }
}
