use super::*;

fn big(lines: impl Iterator<Item = String>) -> String {
    lines.map(|line| line + "\n").collect()
}

#[test]
fn cargo_progress_is_dropped_and_diagnostics_survive() {
    let mut text = big((0..60).map(|i| format!("   Compiling crate{i} v0.{i}.0")));
    text.push_str("warning: unused variable: `x`\n --> src/lib.rs:3:9\n  |\n3 |     let x = 1;\n  |         ^ help: prefix it\n");
    text.push_str("error[E0308]: mismatched types\n --> src/main.rs:1:1\n");
    text.push_str("    Finished `dev` profile in 3.0s\n");
    let out = minimize("cargo build --workspace", &text).expect("shrinks");
    assert_eq!(out.filter, "cargo");
    assert!(!out.text.contains("Compiling"));
    for kept in [
        "warning: unused variable",
        "--> src/lib.rs:3:9",
        "error[E0308]",
        "Finished",
    ] {
        assert!(out.text.contains(kept), "{kept}: {}", out.text);
    }
    assert_eq!((out.before, out.after), (text.len(), out.text.len()));
}

#[test]
fn passing_cargo_tests_collapse_but_failures_and_summary_stay() {
    let mut text = big((0..80).map(|i| format!("test module::case_{i} ... ok")));
    text.push_str("test module::broken ... FAILED\n\nfailures:\n\n---- module::broken stdout ----\nthread 'module::broken' panicked at src/lib.rs:9:5:\nboom\n\ntest result: FAILED. 80 passed; 1 failed; 0 ignored\n");
    let out = minimize("cargo test -p thing", &text).unwrap();
    assert!(
        out.text.contains("[80 passing lines omitted"),
        "{}",
        out.text
    );
    assert!(out.text.contains("test module::broken ... FAILED"));
    assert!(out.text.contains("panicked at src/lib.rs:9:5"));
    assert!(
        out.text
            .contains("test result: FAILED. 80 passed; 1 failed")
    );
    assert!(!out.text.contains("case_3 ... ok"));
}

#[test]
fn short_runs_of_passing_lines_are_kept() {
    let mut text = big((0..40).map(|i| format!("   Compiling crate{i} v1.0.{i}")));
    text.push_str("test a ... ok\ntest b ... ok\ntest result: ok. 2 passed\n");
    let out = minimize("cargo test", &text).unwrap();
    assert!(out.text.contains("test a ... ok\ntest b ... ok\n"));
}

#[test]
fn a_passing_line_that_mentions_failure_is_never_collapsed() {
    let mut text = big((0..60).map(|i| format!("   Compiling crate{i} v1.0.{i}")));
    text.push_str(&big(
        (0..10).map(|i| format!("test handles_failure_{i} ... ok"))
    ));
    let out = minimize("cargo test", &text).unwrap();
    assert_eq!(out.text.matches("handles_failure_").count(), 10);
}

#[test]
fn only_a_single_simple_command_is_filtered() {
    let text = big((0..60).map(|i| format!("   Compiling crate{i} v1.0.{i}")));
    for command in [
        "cargo build && echo done",
        "cargo build; ls",
        "cargo build | tee log",
        "cargo build > log",
        "echo $(cargo build)",
        "cargo run -- --verbose",
        "make",
        "cargo",
        "cargo --version",
    ] {
        // Executed applications have arbitrary stdout, even if it resembles Cargo progress.
        assert!(minimize(command, &text).is_none(), "{command}");
    }
    for command in [
        "cargo build",
        "FOO=1 cargo build 2>&1",
        "cargo +nightly check --all",
        "/usr/bin/cargo test",
    ] {
        assert!(minimize(command, &text).is_some(), "{command}");
    }
}

#[test]
fn unrecognised_commands_only_lose_terminal_noise() {
    let mut text = String::new();
    for i in 0..50 {
        text.push_str(&format!("\u{1b}[2K\rdownloading {i}%\r"));
    }
    text.push_str("\u{1b}[32mdone\u{1b}[0m\n");
    let out = minimize("curl -O https://example.test/x", &text).unwrap();
    assert_eq!(out.filter, "terminal");
    assert_eq!(out.text, "done\n");
}

#[test]
fn redrawn_frames_keep_a_diagnostic() {
    let mut text = String::new();
    for i in 0..60 {
        text.push_str(&format!("progress {i}/60 ...... \r"));
    }
    text.push_str("error: connection reset\rretrying 1\n");
    let out = minimize("some-tool", &text).unwrap();
    assert!(out.text.contains("error: connection reset"), "{}", out.text);
    assert!(out.text.contains("retrying 1"));
    assert!(!out.text.contains("progress 3/60"));
}

#[test]
fn small_savings_and_plain_output_are_left_alone() {
    assert!(minimize("cargo build", "   Compiling a v1.0.0\nFinished\n").is_none());
    let plain = big((0..100).map(|i| format!("line {i} of ordinary output")));
    assert!(minimize("cargo build", &plain).is_none());
    assert!(minimize("ls", &plain).is_none());
}

#[test]
fn git_hints_and_transport_progress_are_dropped() {
    let mut text =
        big((0..30).map(|i| format!("hint: long advice line number {i} about branch names")));
    text.push_str("Switched to branch 'x'\nfatal: something\n");
    let out = minimize("git -C sub checkout x", &text).unwrap();
    assert_eq!(out.filter, "git");
    assert!(!out.text.contains("hint:"));
    assert!(out.text.contains("fatal: something"));

    let mut push = big((0..40).map(|i| format!("Compressing objects: {i}% ({i}/40)")));
    push.push_str(&big((0..40).map(|_| {
        "Writing objects: 100% (4/4), 400 bytes | 400.00 KiB/s, done.".to_owned()
    })));
    push.push_str("To github.com:o/r.git\n   abc..def  main -> main\n");
    let out = minimize("git push origin main", &push);
    assert!(out.is_none() || out.unwrap().text.contains("main -> main"));

    let mut status = big((0..20)
        .map(|_| "  (use \"git add <file>...\" to update what will be committed)".to_owned()));
    status.push_str("On branch main\n\tmodified:   src/lib.rs\n");
    status.push_str(&"x".repeat(100));
    let out = minimize("git status", &status).unwrap();
    assert!(out.text.contains("modified:   src/lib.rs") && !out.text.contains("(use"));
    // Content commands are never filtered.
    assert!(minimize("git diff", &status).is_none());
    assert!(minimize("git log", &status).is_none());
}

#[test]
fn package_install_noise_is_dropped() {
    let mut text =
        big((0..30).map(|i| format!("npm notice Changelog for package {i} is available")));
    text.push_str("added 120 packages in 4s\n\n12 packages are looking for funding\n  run `npm fund` for details\nnpm ERR! code E404\n");
    for command in ["npm install", "pnpm add left-pad", "yarn"] {
        let out = minimize(command, &text).unwrap();
        assert!(out.text.contains("added 120 packages") && out.text.contains("npm ERR! code E404"));
        assert!(!out.text.contains("funding") && !out.text.contains("npm notice"));
    }
    assert!(minimize("npm run build", &text).is_none());
    assert!(minimize("yarn test", &text).is_none());
}

#[test]
fn pytest_and_go_test_collapse_passes() {
    let mut py = String::from(
        "============ test session starts ============\nplatform darwin -- Python 3.12\nrootdir: /w\nplugins: x\n",
    );
    py.push_str(&big((0..60).map(|i| {
        format!("tests/test_a.py::test_case_{i} PASSED          [{i}%]")
    })));
    py.push_str("tests/test_a.py::test_bad FAILED\n=== FAILURES ===\nE   assert 1 == 2\n=== 1 failed, 60 passed in 1.2s ===\n");
    let out = minimize("python -m pytest -v", &py).unwrap();
    assert_eq!(out.filter, "pytest");
    for kept in [
        "test_bad FAILED",
        "E   assert 1 == 2",
        "1 failed, 60 passed",
    ] {
        assert!(out.text.contains(kept), "{kept}");
    }
    assert!(!out.text.contains("test_case_7 PASSED") && !out.text.contains("rootdir"));

    let mut go = String::new();
    for i in 0..40 {
        go.push_str(&format!(
            "=== RUN   TestCase{i}\n--- PASS: TestCase{i} (0.00s)\n"
        ));
    }
    go.push_str("--- FAIL: TestBroken (0.00s)\n    x_test.go:5: got 1\nFAIL\nFAIL\tpkg\t0.01s\n");
    let out = minimize("go test ./...", &go).unwrap();
    assert!(!out.text.contains("=== RUN") && !out.text.contains("TestCase7"));
    assert!(out.text.contains("--- FAIL: TestBroken") && out.text.contains("x_test.go:5: got 1"));
}

#[test]
fn filtering_never_removes_a_diagnostic_line() {
    // Property over a mixed bag: every diagnostic line of the cleaned input is in the output.
    let mut text = String::new();
    for i in 0..120 {
        text.push_str(&match i % 6 {
            0 => format!("   Compiling c{i} v1.0.0\n"),
            1 => format!("test t{i} ... ok\n"),
            2 => format!("warning: thing {i}\n"),
            3 => format!("Traceback line {i}\n"),
            4 => format!("test t{i} ... FAILED\n"),
            _ => format!("note {i}\n"),
        });
    }
    let out = minimize("cargo test", &text).unwrap();
    assert_eq!(diagnostic_lines(&out.text), diagnostic_lines(&text));
}

#[test]
fn clippy_checking_progress_drops_but_failure_diagnostic_and_status_stay() {
    let mut text =
        big((0..80).map(|i| format!("    Checking dependency{i} v0.1.{i} (/workspace/crate{i})")));
    text.push_str("error: this expression creates a reference which is immediately dereferenced\n --> src/lib.rs:4:5\n  |\n4 |     &value\n  |     ^^^^^^ help: change this to: `value`\n  = note: `-D clippy::needless-borrow` implied by `-D warnings`\nerror: could not compile `app` (lib) due to 1 previous error\n");
    let out = minimize("cargo clippy --workspace -- -D warnings", &text).unwrap();
    assert!(!out.text.contains("Checking dependency"));
    assert_eq!(out.text, &text[text.find("error:").unwrap()..]);
}

#[test]
fn homogeneous_cargo_sequences_compact_without_losing_summary_or_coverage() {
    let mut text = big((0..60).map(|i| format!("    Checking dependency{i} v1.0.0")));
    text.push_str(&big(
        (0..80).map(|i| format!("test module::case_{i} ... ok"))
    ));
    text.push_str("warning: keep this warning\n    Finished `test` profile in 1s\nrunning 80 tests\ntest result: ok. 80 passed; 0 failed; 2 ignored; 3 filtered out\n");
    for command in [
        "cargo fmt --check && cargo check && cargo test",
        "cd workspace && cargo check; cargo test",
        "cargo check\ncargo test",
    ] {
        let out = minimize(command, &text).expect(command);
        assert_eq!(out.filter, "cargo");
        assert!(!out.text.contains("Checking dependency"), "{command}");
        assert!(out.text.contains("[80 passing lines omitted"));
        for kept in [
            "warning: keep",
            "Finished",
            "running 80 tests",
            "80 passed; 0 failed; 2 ignored; 3 filtered out",
        ] {
            assert!(out.text.contains(kept), "{command}: {kept}");
        }
    }
    for command in [
        "cargo check && cat output.txt",
        "cargo check && cargo run",
        "cargo check && cargo bench",
        "cargo check | cargo test",
        "cargo check & cargo test",
        "(cargo check; cargo test)",
        "echo 'cargo check && cargo test'",
        "cargo check && printf 'test example ... ok'",
    ] {
        assert!(minimize(command, &text).is_none(), "{command}");
    }
}
