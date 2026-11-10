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
fn a_cargo_failure_buried_under_progress_gets_a_digest_of_its_first_errors() {
    let mut text = big((0..200).map(|i| format!("   Compiling crate{i} v0.{i}.0")));
    text.push_str("warning: unused variable: `x`\n --> src/lib.rs:3:9\n");
    text.push_str("error[E0308]: mismatched types\n --> src/main.rs:7:5\n  |\n7 |     1u8\n");
    text.push_str("error: could not compile `app` (bin \"app\") due to 1 previous error\n");
    let digest = failure_digest("cargo build", &text, &text).expect("digest");
    assert_eq!(
        digest,
        "error[E0308]: mismatched types\n --> src/main.rs:7:5\nerror: could not compile `app` (bin \"app\") due to 1 previous error\n"
    );
    // Warnings' locations are not errors; unknown commands and visible errors get none.
    assert!(!digest.contains("src/lib.rs"));
    assert!(failure_digest("./run.sh", &text, &text).is_none());
    let top = format!(
        "error[E0308]: mismatched types\n --> src/main.rs:7:5\nerror: could not compile `app` (bin \"app\") due to 1 previous error\n{text}"
    );
    assert!(failure_digest("cargo build", &top, &top).is_none());
    assert!(failure_digest("cargo build", "error: short\n", "error: short\n").is_none());
    let piped = failure_digest("cargo build | rg result", &text, "no matches")
        .expect("captured pipeline diagnostics");
    assert!(piped.contains("error[E0308]"));
    assert!(piped.contains("src/main.rs:7:5"));
}

#[test]
fn failing_tests_are_digested_with_their_panic_detail() {
    let mut text = big((0..100).map(|i| format!("   Compiling crate{i} v0.{i}.0")));
    text.push_str(&big(
        (0..80).map(|i| format!("test module::case_{i} ... ok"))
    ));
    text.push_str(concat!(
        "test module::broken ... FAILED\n\nfailures:\n\n",
        "---- module::broken stdout ----\n",
        "thread 'module::broken' panicked at src/lib.rs:42:9:\n",
        "assertion `left == right` failed\n  left: 1\n right: 2\n",
        "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\n",
        "failures:\n    module::broken\n\n",
        "test result: FAILED. 80 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    ));
    // The shown text is a filtered view that lost nothing, but the failure sits far down.
    let digest = failure_digest("cargo test", &text, &text).expect("digest");
    assert_eq!(
        digest,
        "---- module::broken stdout ----\nthread 'module::broken' panicked at src/lib.rs:42:9:\nassertion `left == right` failed\n  left: 1\n right: 2\ntest result: FAILED. 80 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"
    );
    // A head-and-tail view that cut the panic out still gets it from the complete output.
    let shown = "running 81 tests\n[40000 bytes of output omitted]\ntest result: FAILED. 80 passed; 1 failed\n";
    let digest = failure_digest("cargo test", &text, shown).expect("digest");
    assert!(digest.contains("panicked at src/lib.rs:42:9"), "{digest}");

    let mut pytest = big((0..200).map(|i| format!("tests/test_{i}.py ....  [{i:>3}%]")));
    pytest.push_str(concat!(
        "=================================== FAILURES ===================================\n",
        "_________________________________ test_parse _________________________________\n\n",
        "    def test_parse():\n>       assert parse('x') == 2\nE       AssertionError: assert 1 == 2\n\n",
        "tests/test_parse.py:7: AssertionError\n",
        "=========================== short test summary info ============================\n",
        "FAILED tests/test_parse.py::test_parse - AssertionError: assert 1 == 2\n",
        "========================= 1 failed, 319 passed in 2.10s =========================\n",
    ));
    let digest = failure_digest("pytest", &pytest, &pytest).expect("digest");
    assert!(
        digest.starts_with("_________________________________ test_parse"),
        "{digest}"
    );
    assert!(
        digest.contains("E       AssertionError: assert 1 == 2"),
        "{digest}"
    );
    assert!(
        digest.contains("tests/test_parse.py:7: AssertionError"),
        "{digest}"
    );
    assert!(
        digest.ends_with("1 failed, 319 passed in 2.10s =========================\n"),
        "{digest}"
    );
}

#[test]
fn silent_exit_codes_with_a_conventional_meaning_are_explained() {
    for (command, meaning) in [
        ("grep -rn needle src", "no lines matched"),
        ("rg needle", "no lines matched"),
        ("git grep needle", "no lines matched"),
        ("git diff --quiet", "there are differences"),
        ("diff a b", "the inputs differ"),
        ("test -f missing", "the condition is false"),
    ] {
        assert_eq!(silent_exit_meaning(command, 1), Some(meaning), "{command}");
    }
    for (command, code) in [
        ("grep needle src", 2),
        ("git diff", 1),
        ("cargo build", 1),
        ("grep a | wc -l", 1),
    ] {
        assert_eq!(silent_exit_meaning(command, code), None, "{command} {code}");
    }
}

#[test]
fn an_earlier_failure_masked_by_a_zero_exit_is_named() {
    let missing = "Fetched 142 kB\nsh: 1: dpkg-source: not found\narchive.tar.xz\n";
    let masked = masked_failure("fetch-source pkg 2>&1; ls", missing).unwrap();
    assert_eq!(masked, "sh: 1: dpkg-source: not found");
    assert!(masked_failure("./configure; make", "bash: autoreconf: command not found\n").is_some());
    let build = "cc -c main.c\nerror: linker `cc` not found\nmain.o\n";
    assert!(
        masked_failure("make 2>&1 | tail -5", build)
            .unwrap()
            .starts_with("error: linker")
    );
    // A sole command's zero status is its own; ordinary output names no failure.
    assert_eq!(masked_failure("fetch-source pkg", missing), None);
    assert_eq!(
        masked_failure("make 2>&1; ls", "cc -O main.c\nmain.o\n"),
        None
    );
    assert_eq!(
        masked_failure(
            "grep -rn 'error: ' src | head",
            "src/a.rs:3: let x = \"error: \";\n"
        ),
        None
    );
    // Query results whose first column is an `error:` label are data, not diagnostics.
    assert_eq!(
        masked_failure(
            "sqlite3 s.db 'select finish, count(*) from requests group by 1'; echo done",
            "tool_calls|4990\nerror:interrupted|30\nerror:fatal|6\ndone\n"
        ),
        None
    );
    assert!(masked_failure("make; ls", "error[E0063]: missing field\n").is_some());
    let long = format!("error: {}\n", "x".repeat(400));
    assert!(masked_failure("make; ls", &long).unwrap().chars().count() <= DIGEST_LINE_CHARS);
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
fn apt_progress_is_dropped_but_summary_and_errors_stay() {
    let mut text = String::from(
        "Reading package lists...\nBuilding dependency tree...\nThe following NEW packages will be installed:\n  binutils gcc gcc-13 make\n0 upgraded, 4 newly installed, 0 to remove and 0 not upgraded.\nNeed to get 30.1 MB of archives.\n",
    );
    for i in 0..40 {
        text.push_str(&format!(
            "Get:{i} http://archive.ubuntu.com/ubuntu noble/main amd64 pkg{i} 1.0 [100 kB]\nSelecting previously unselected package pkg{i}.\nPreparing to unpack .../pkg{i}_1.0_amd64.deb ...\nUnpacking pkg{i} (1.0) ...\nSetting up pkg{i} (1.0) ...\n"
        ));
    }
    text.push_str("E: Unable to locate package nosuch\n");
    for command in [
        "apt-get install -y gcc make",
        "apt-get update -qq && apt-get install -y gcc make 2>&1",
        "DEBIAN_FRONTEND=noninteractive apt install -y gcc",
    ] {
        let out = minimize(command, &text).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(out.filter, "apt", "{command}");
        assert!(out.text.contains("4 newly installed") && out.text.contains("E: Unable to locate"));
        assert!(!out.text.contains("Unpacking") && !out.text.contains("  binutils"));
    }
    assert!(minimize("apt-cache show gcc", &text).is_none());
    assert!(minimize("apt list --installed", &text).is_none());
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

#[test]
fn pip_install_and_js_test_noise_is_filtered_behind_a_cd() {
    let mut pip = big((0..40).map(|i| format!("Collecting package{i}>=1.0")));
    pip.push_str(&big((0..40).map(|i| {
        format!("  Using cached package{i}-1.0-py3-none-any.whl (12 kB)")
    })));
    pip.push_str(
        "ERROR: pip's dependency resolver found a conflict\nSuccessfully installed package0-1.0\n",
    );
    for command in [
        "pip install -r requirements.txt",
        "python3 -m pip install .",
        "uv pip install -e .",
        "cd backend && pip install -r requirements.txt",
    ] {
        let out = minimize(command, &pip).expect(command);
        assert_eq!(out.filter, "pip install", "{command}");
        assert!(!out.text.contains("Collecting"), "{command}");
        assert!(out.text.contains("ERROR: pip's dependency resolver"));
        assert!(out.text.contains("Successfully installed"));
    }

    let mut jest = big((0..60).map(|i| format!("PASS src/module{i}.test.ts")));
    jest.push_str("FAIL src/broken.test.ts\nTests: 1 failed, 60 passed, 61 total\n");
    for command in ["npx jest", "cd web && npx vitest run", "vitest run"] {
        let out = minimize(command, &jest).expect(command);
        assert_eq!(out.filter, "js test", "{command}");
        assert!(out.text.contains("[60 passing lines omitted"), "{command}");
        assert!(out.text.contains("FAIL src/broken.test.ts"));
        assert!(out.text.contains("Tests: 1 failed"));
    }
    // Different recognized tools in one compound command keep their raw output.
    assert!(minimize("pip install x && npx jest", &jest).is_none());
}

#[test]
fn a_fully_passing_test_run_becomes_one_summary_line() {
    let mut text = big((0..30).map(|i| format!("   Compiling crate{i} v0.{i}.0")));
    text.push_str("    Finished `test` profile [unoptimized] target(s) in 9.1s\n     Running unittests src/lib.rs (target/debug/deps/a-1)\n\nrunning 40 tests\n");
    text.push_str(&big(
        (0..40).map(|i| format!("test module::case_{i} ... ok"))
    ));
    text.push_str("\ntest result: ok. 40 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n   Doc-tests a\n\nrunning 1 test\ntest src/lib.rs - f (line 3) ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s\n");
    let out = minimize("cargo test -p a", &text).expect("summarized");
    assert_eq!(out.filter, "cargo");
    assert_eq!(
        out.text,
        "test result: ok. 41 passed; 0 failed; 2 ignored (2 suites; passing run summarized by the output filter)\n"
    );

    let mut pytest = big((0..40).map(|i| format!("tests/test_{i}.py ....  [{i:>3}%]")));
    pytest.push_str("============ 160 passed, 3 skipped in 4.21s ============\n");
    let out = minimize("python -m pytest", &pytest).expect("summarized");
    assert!(
        out.text
            .starts_with("test result: ok. 160 passed; 0 failed; 3 ignored (1 suite;"),
        "{}",
        out.text
    );

    let mut jest = big((0..60).map(|i| format!("PASS src/module{i}.test.ts")));
    jest.push_str("Tests:       2 skipped, 118 passed, 120 total\n");
    let out = minimize("npx jest", &jest).expect("summarized");
    assert!(
        out.text
            .starts_with("test result: ok. 118 passed; 0 failed; 2 ignored"),
        "{}",
        out.text
    );
}

#[test]
fn runs_with_any_other_diagnostic_keep_the_line_filter() {
    let passing = big((0..40).map(|i| format!("test module::case_{i} ... ok")));
    let summary = "test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured\n";
    for extra in [
        "warning: unused variable: `x`\n --> src/lib.rs:3:9\n",
        "test module::broken ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n",
        "test module::handles_failure ... ok\n",
    ] {
        let text = format!("{passing}{extra}{summary}");
        let out = minimize("cargo test", &text).expect("still filtered");
        assert!(
            !out.text.contains("summarized by the output filter"),
            "{extra}"
        );
        assert!(out.text.contains(extra.lines().next().unwrap()), "{extra}");
    }
    // No runner summary at all (a build) is never summarized as a test pass.
    let build = big((0..60).map(|i| format!("   Compiling crate{i} v1.0.{i}")));
    let out = minimize("cargo build", &build).unwrap();
    assert!(!out.text.contains("test result"), "{}", out.text);
}
