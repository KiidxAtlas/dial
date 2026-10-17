use super::*;

#[test]
fn command_verdict_uses_typed_completion_and_raw_stdout() {
    let complete = ProcessOutcome {
        started: true,
        exit_code: Some(3),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    };
    assert!(command_matches(&complete, b"42\n", Some("42"), 3));
    assert!(command_matches(&complete, b"", Some(""), 3));
    assert!(!command_matches(&complete, b"[exit code: 0]\n", None, 0));
    assert!(!command_matches(
        &complete,
        b"42\n[exit code: 3]\n",
        Some("42"),
        3
    ));
    for incomplete in [
        ProcessOutcome {
            started: false,
            ..complete.clone()
        },
        ProcessOutcome {
            cancelled: true,
            ..complete.clone()
        },
        ProcessOutcome {
            timed_out: true,
            ..complete.clone()
        },
        ProcessOutcome {
            output_complete: false,
            ..complete.clone()
        },
        ProcessOutcome {
            signal: Some(9),
            ..complete.clone()
        },
        ProcessOutcome {
            exit_code: None,
            ..complete.clone()
        },
    ] {
        assert!(!command_matches(&incomplete, b"42", Some("42"), 3));
    }
}

#[test]
fn checks_judge_final_state_not_intent() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    fs::write(dir.path().join("c.json"), "{\"k\":{\"n\":7}}").unwrap();
    let w = dir.path();
    assert!(
        Check::FileEquals {
            path: "a.txt".into(),
            content: "hello".into()
        }
        .verify(w, "")
    );
    assert!(
        !Check::FileEquals {
            path: "a.txt".into(),
            content: "bye".into()
        }
        .verify(w, "")
    );
    assert!(
        Check::JsonFile {
            path: "c.json".into(),
            pointer: "/k/n".into(),
            expected: json!(7)
        }
        .verify(w, "")
    );
    assert!(
        Check::FileAbsent {
            path: "nope".into()
        }
        .verify(w, "")
    );
    assert!(
        Check::AnswerExact {
            expected: "x".into()
        }
        .verify(w, " x \n")
    );
    assert!(
        Check::AnswerJson {
            expected: json!([1, 2])
        }
        .verify(w, "[1,2]")
    );
    assert!(
        !Check::All { checks: vec![] }.verify(w, ""),
        "an empty All must not pass vacuously"
    );
}

#[cfg(unix)]
#[test]
fn only_a_definite_not_found_proves_file_absence() {
    use std::os::unix::fs::PermissionsExt;
    // Root reads through directories without search permission, so only the absence
    // assertion is meaningful there.
    // SAFETY: geteuid has no preconditions and cannot fail.
    let privileged = unsafe { libc::geteuid() } == 0;
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path();
    fs::write(w.join("present.txt"), "x").unwrap();
    assert!(
        Check::FileAbsent {
            path: "missing.txt".into()
        }
        .verify(w, "")
    );
    assert!(
        Check::FileAbsent {
            path: "missing/child.txt".into()
        }
        .verify(w, "")
    );
    assert!(
        !Check::FileAbsent {
            path: "present.txt".into()
        }
        .verify(w, "")
    );
    std::os::unix::fs::symlink(w.join("present.txt"), w.join("link")).unwrap();
    assert!(
        !Check::FileAbsent {
            path: "link".into()
        }
        .verify(w, ""),
        "symlink components are rejected rather than treated as absent"
    );
    // Metadata for a child of a non-searchable directory fails with PermissionDenied.
    fs::create_dir(w.join("locked")).unwrap();
    fs::write(w.join("locked/secret.txt"), "x").unwrap();
    fs::set_permissions(w.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let absent = Check::FileAbsent {
        path: "locked/secret.txt".into(),
    }
    .verify(w, "");
    let readable = Check::FileContains {
        path: "locked/secret.txt".into(),
        needle: "x".into(),
    }
    .verify(w, "");
    // Restore access before any assertion can fail, so the temporary directory cleans up.
    fs::set_permissions(w.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!absent, "an inaccessible file must not count as deleted");
    assert!(privileged || !readable);
}

#[cfg(unix)]
#[test]
fn paths_cannot_escape_or_follow_symlinks() {
    let outer = tempfile::tempdir().unwrap();
    let dir = outer.path().join("w");
    fs::create_dir(&dir).unwrap();
    fs::write(outer.path().join("secret"), "s").unwrap();
    std::os::unix::fs::symlink(outer.path().join("secret"), dir.join("link")).unwrap();
    assert!(
        !Check::FileContains {
            path: "../secret".into(),
            needle: "s".into()
        }
        .verify(&dir, "")
    );
    assert!(
        !Check::FileContains {
            path: "link".into(),
            needle: "s".into()
        }
        .verify(&dir, "")
    );
    assert!(
        !Check::FileContains {
            path: "/etc/hosts".into(),
            needle: "".into()
        }
        .verify(&dir, "")
    );
}

#[test]
fn command_check_runs_in_the_sandbox() {
    if !crate::tools::exec::sandbox::require_for_test() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("t.py"), "print(6*7)\n").unwrap();
    let ok = Check::Command {
        command: "python3 t.py; printf 'trailing diagnostic [exit code: 99]\\n' >&2".into(),
        expect_stdout: Some("42".into()),
        expect_exit: 0,
    };
    assert!(
        ok.verify(dir.path(), ""),
        "raw sandbox command output: {}",
        {
            let ws = Workspace::new(dir.path())
                .unwrap()
                .with_policy(Policy::Strict {
                    allow_network: false,
                    deny_read: Vec::new(),
                    extra_write: Vec::new(),
                });
            tools::execute(
                &ws,
                "bash",
                &json!({"command":"python3 t.py","timeout":30}),
                30,
            )
            .text
        }
    );
    assert!(
        !Check::Command {
            command: "python3 t.py".into(),
            expect_stdout: Some("41".into()),
            expect_exit: 0
        }
        .verify(dir.path(), "")
    );
    assert!(
        Check::Command {
            command: "exit 3".into(),
            expect_stdout: None,
            expect_exit: 3
        }
        .verify(dir.path(), "")
    );
    // A program can print convincing status/timeout text; only actual completion is proof.
    assert!(
        Check::Command {
            command: "printf '[exit code: 99]\\n'; exit 3".into(),
            expect_stdout: Some("[exit code: 99]".into()),
            expect_exit: 3,
        }
        .verify(dir.path(), "")
    );
    assert!(
        !Check::Command {
            command: "printf '[exit code: 0]\\n'; exit 3".into(),
            expect_stdout: None,
            expect_exit: 0,
        }
        .verify(dir.path(), "")
    );
    assert!(
        !Check::Command {
            command: "head -c 4194305 /dev/zero".into(),
            expect_stdout: None,
            expect_exit: 0,
        }
        .verify(dir.path(), "")
    );
    assert!(
        !Check::Command {
            command: "kill -TERM $$".into(),
            expect_stdout: None,
            expect_exit: 0,
        }
        .verify(dir.path(), "")
    );
}
