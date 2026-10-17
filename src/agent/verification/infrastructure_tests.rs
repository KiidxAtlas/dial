use super::*;

fn verdict(name: &str, command: &str, diagnostic: &str) -> CheckStatus {
    let check = FocusedCheck {
        name: name.into(),
        command: command.into(),
        timeout_secs: 0,
    };
    let output = crate::tools::ToolOutput {
        execution: Default::default(),
        text: format!("{diagnostic}\n[exit code: 1]"),
        is_error: true,
        data: None,
    };
    let output = output.with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: Some(1),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    classify(&check, &output, false)
}

#[test]
fn missing_selected_toolchains_are_unavailable() {
    for (name, command, diagnostic) in [
        (
            "cargo check",
            "cargo check --offline",
            "bash: cargo: command not found",
        ),
        (
            "related Rust tests: app::tests::",
            "cargo test --offline --lib",
            "sh: 1: cargo: not found",
        ),
        (
            "cargo check",
            "cargo check --offline",
            "error: rustup could not choose a version of cargo to run, because one wasn't specified explicitly, and no default is configured.",
        ),
        (
            "related Go package tests: ./app",
            "go test './app'",
            "bash: go: command not found",
        ),
        (
            "related vitest test: app.test.ts",
            "'/project/node_modules/.bin/vitest' run 'app.test.ts'",
            "/usr/bin/env: node: No such file or directory",
        ),
        (
            "related jest test: app.test.js",
            "'/project/node_modules/.bin/jest' --runInBand 'app.test.js'",
            "env: node: No such file or directory",
        ),
    ] {
        assert_eq!(
            verdict(name, command, diagnostic),
            CheckStatus::EnvironmentUnavailable,
            "{diagnostic}"
        );
    }
}

#[test]
fn offline_cargo_cache_absence_is_unavailable() {
    for diagnostic in [
        "error: no matching package named `serde` found\nlocation searched: crates.io index\nrequired by package `app v0.1.0`\nAs a reminder, you're using offline mode (--offline) which can sometimes cause surprising resolution failures",
        "error: failed to download `serde v1.0.0`\nCaused by:\n  attempting to make an HTTP request, but --offline was specified",
    ] {
        assert_eq!(
            verdict(
                "cargo check",
                "cargo check --offline --all-targets",
                diagnostic
            ),
            CheckStatus::EnvironmentUnavailable,
            "{diagnostic}"
        );
        assert_eq!(
            verdict(
                "related Rust tests: app::tests::",
                "cargo test --offline --lib",
                diagnostic
            ),
            CheckStatus::EnvironmentUnavailable
        );
        assert_eq!(
            verdict("cargo check", "cargo check", diagnostic),
            CheckStatus::CodeFailed
        );
    }
}

#[test]
fn project_defects_are_not_setup_failures() {
    for (name, command, diagnostic) in [
        (
            "cargo check",
            "cargo check --offline",
            "error[E0308]: mismatched types",
        ),
        (
            "cargo check",
            "cargo check --offline",
            "error: expected `;`, found `}`",
        ),
        (
            "cargo check",
            "cargo check --offline",
            "error: failed to select a version for the requirement `serde = \"^99\"`",
        ),
        (
            "cargo check",
            "cargo check --offline",
            "error: no matching package named `app` found\nlocation searched: /project/app",
        ),
        (
            "related Rust tests: app::tests::",
            "cargo test --offline --lib",
            "thread 'app::tests::answer' panicked: assertion `left == right` failed",
        ),
        (
            "related Go package tests: ./app",
            "go test './app'",
            "app/main.go:3: undefined: answer",
        ),
        (
            "related vitest test: app.test.ts",
            "'/project/node_modules/.bin/vitest' run",
            "SyntaxError: Unexpected token",
        ),
        (
            "related jest test: app.test.js",
            "'/project/node_modules/.bin/jest' --runInBand",
            "Cannot find module './app' from 'app.test.js'",
        ),
        (
            "related Python tests: app",
            "python -m pytest",
            "ModuleNotFoundError: No module named 'app'",
        ),
        (
            "related Python tests: app",
            "python -m pytest",
            "AssertionError: wrong answer",
        ),
        (
            "python AST: app.py",
            "python -c 'import ast'",
            "SyntaxError: invalid syntax",
        ),
        (
            "related Python tests: app",
            "python -m pytest",
            "bash: cargo: command not found",
        ),
        (
            "cargo check",
            "echo cargo",
            "bash: cargo: command not found",
        ),
    ] {
        assert_eq!(
            verdict(name, command, diagnostic),
            CheckStatus::CodeFailed,
            "{diagnostic}"
        );
    }
}
