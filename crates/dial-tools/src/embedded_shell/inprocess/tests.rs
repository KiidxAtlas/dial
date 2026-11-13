use super::*;

#[cfg(unix)]
#[test]
fn leaders_are_direct_children_that_lead_their_group() {
    let ps = "  10   1  10\n  11  10  11\n  12  10  10\n  13  11  13\n bad row\n";
    assert_eq!(parse_leaders(ps, 10), vec![11]);
    assert_eq!(parse_leaders(ps, 11), vec![13]);
    assert!(parse_leaders(ps, 99).is_empty());
}

#[cfg(unix)]
#[test]
fn token_discovery_ignores_disappeared_candidates_and_other_rows() {
    let rows = "10 dial DIAL_INPROCESS_RUN=token\n11 command DIAL_INPROCESS_RUN=token-extra\n12 command DIAL_INPROCESS_RUN=token\n";
    assert!(!token_in_ps(rows, 99, "DIAL_INPROCESS_RUN=token"));
    assert!(!token_in_ps(rows, 11, "DIAL_INPROCESS_RUN=token"));
    assert!(token_in_ps(rows, 12, "DIAL_INPROCESS_RUN=token"));
}

#[cfg(feature = "embedded-shell")]
#[test]
fn run_reports_exit_code_and_writes_output() {
    use std::io::Read;
    crate::llm::fake::raise_fd_limit();
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut run = Run::start(Request {
        script: "echo hi; exit 3".into(),
        cwd: std::env::temp_dir(),
        env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
        output: writer,
    })
    .unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    assert_eq!(out, "hi\n");
    while !run.exited() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run.code(), Some(3));
    assert!(run.terminate().shell_stopped);
}

#[cfg(feature = "embedded-shell")]
#[test]
fn portable_pipeline_uses_bundled_utilities_without_host_path() {
    use std::io::Read;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input file.txt"), "from cat\n").unwrap();
    crate::llm::fake::raise_fd_limit();
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut run = Run::start(Request {
        script: "set -o pipefail; printf 'pear\\napple\\npear\\n' | sort -u | grep -F apple | sed -n '1p'; cat 'input file.txt'".into(),
        cwd: dir.path().to_path_buf(),
        env: vec![("PATH".into(), String::new())],
        output: writer,
    })
    .unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    while !run.exited() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run.code(), Some(0), "{out}");
    assert_eq!(out, "apple\nfrom cat\n");
}

/// Runs `script` through the embedded engine and returns its exit code and captured output.
#[cfg(feature = "embedded-shell")]
fn run_script(script: &str) -> (Option<i32>, String) {
    use std::io::Read;

    // Parallel runs each hold a runtime and pipes; macOS's default 256 descriptors run out.
    crate::llm::fake::raise_fd_limit();
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut run = Run::start(Request {
        script: script.into(),
        cwd: std::env::temp_dir(),
        env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
        output: writer,
    })
    .unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    while !run.exited() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let code = run.code();
    assert!(run.terminate().shell_stopped);
    (code, out)
}

// Uses `sleep` as the long-running external command; on Windows `sleep` and `kill` are bundled
// in-process utilities, so they start no process for `$!` to name.
#[cfg(all(unix, feature = "embedded-shell"))]
#[test]
fn background_job_pid_is_reported_by_dollar_bang() {
    // `$!` names the job's external process: the last member of the first pipeline that starts
    // one, or the job's own process for a subshell job. A job that starts none names itself by a
    // virtual PID, so `$!` is never empty after `&`. It is available right after `&` (the expansion
    // waits for the job to start a process), and survives `wait` clearing the job table. A killed
    // job's process must end, so each kill case runs far longer than the time the test allows (the
    // script only finishes once the background process has closed the output pipe).
    let cases = [
        (
            "sleep 20 & p=$!; case $p in ''|*[!0-9]*) echo bad;; *) echo ok;; esac; kill $p; wait %1; echo done",
            "ok\ndone\n",
        ),
        (
            "(sleep 20) & p=$!; case $p in ''|*[!0-9]*) echo bad;; *) echo ok;; esac; kill $p; wait %1; echo done",
            "ok\ndone\n",
        ),
        ("sleep 20 & p=$!; kill %1; wait %1; echo done", "done\n"),
        ("sleep 1 & wait; [ -n \"$!\" ] && echo kept", "kept\n"),
        (
            "true & p=$!; case $p in ''|*[!0-9]*) echo bad;; *) echo ok;; esac",
            "ok\n",
        ),
    ];
    for (script, expected) in cases {
        let started = std::time::Instant::now();
        let (code, out) = run_script(script);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{script}: background process was not stopped"
        );
        assert_eq!(code, Some(0), "{script}: {out}");
        assert_eq!(out, expected, "{script}");
    }
}

#[cfg(feature = "embedded-shell")]
#[test]
fn background_job_without_external_process_reports_a_virtual_pid() {
    // A builtin-only job never starts an external process; `$!` is still its PID, a virtual one,
    // so it is never empty. The loop is short so the test stays quick.
    let (code, out) = run_script(
        "i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done & p=$!; case $p in ''|*[!0-9]*) echo bad;; *) echo ok;; esac; wait $p; echo \"status=$?\"",
    );
    assert_eq!(code, Some(0));
    assert_eq!(out, "ok\nstatus=0\n");
}

#[test]
fn termination_caches_cleanup_failure_independently_of_shell_completion() {
    let (sender, done) = std::sync::mpsc::channel();
    sender.send(0).unwrap();
    let mut run = controlled_run(done);
    let result = run.terminate_with(|_, _| Err("discovery failed".into()));
    assert!(result.shell_stopped);
    assert!(result.uncertain());
    assert_eq!(result.cleanup_error.as_deref(), Some("discovery failed"));
    assert_eq!(
        run.terminate_with(|_, _| panic!("cleanup repeated")),
        result
    );
    // Drop must use the same cached result, not invoke real discovery.
    drop(run);
}

#[test]
fn abandoned_receiver_is_bounded_and_not_retried() {
    let (_sender, done) = std::sync::mpsc::channel();
    let mut run = controlled_run(done);
    let result = run.terminate_with(|_, _| Ok(()));
    assert!(!result.shell_stopped);
    assert!(result.uncertain());
    assert_eq!(
        run.terminate_with(|_, _| panic!("cleanup repeated")),
        result
    );
}

fn controlled_run(done: Receiver<i32>) -> Run {
    Run {
        done,
        code: None,
        token: "controlled".into(),
        #[cfg(windows)]
        started: 0,
        termination: None,
        #[cfg(feature = "embedded-shell")]
        abort: {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.spawn(std::future::pending::<()>()).abort_handle()
        },
    }
}

/// A stopped run terminates the external command it started, found by its environment token.
#[cfg(all(windows, feature = "embedded-shell"))]
#[test]
fn windows_stop_terminates_marked_external_commands() {
    let (_reader, writer) = std::io::pipe().unwrap();
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut run = Run::start(Request {
        script: "ping -n 60 127.0.0.1".into(),
        cwd: std::env::temp_dir(),
        env: vec![
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("SystemRoot".into(), system_root),
        ],
        output: writer,
    })
    .unwrap();
    let entry = format!("{TOKEN_ENV}={}", run.token);
    let deadline = Instant::now() + Duration::from_secs(20);
    let marked = loop {
        let found: Vec<u32> = crate::platform::processes::snapshot()
            .unwrap()
            .into_iter()
            .filter(|p| p.image.eq_ignore_ascii_case("ping.exe"))
            .filter(|p| {
                crate::platform::processes::details(p.pid).is_some_and(|d| d.created >= run.started)
            })
            .map(|p| p.pid)
            .collect();
        if !found.is_empty() {
            break found;
        }
        assert!(Instant::now() < deadline, "ping never started");
        std::thread::sleep(Duration::from_millis(50));
    };
    let result = run.terminate();
    assert_eq!(result.cleanup_error, None);
    for pid in marked {
        assert!(
            crate::platform::processes::details(pid).is_none_or(|d| d.exited),
            "ping {pid} carrying {entry} survived the stop"
        );
    }
}

/// `npx`-style commands: a bare name and a `./name` both run the `PATHEXT` match (`tool.cmd`),
/// never the extensionless POSIX shim beside it (vendor/brush-core/DIAL.md).
#[cfg(all(windows, feature = "embedded-shell"))]
#[test]
fn windows_commands_resolve_through_pathext() {
    use std::io::Read;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tool"), "#!/bin/sh\necho shim\n").unwrap();
    std::fs::write(dir.path().join("tool.cmd"), "@echo cmd %1\r\n").unwrap();
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let path = format!("{};{system_root}\\System32", dir.path().display());
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut run = Run::start(Request {
        script: "tool bare; ./tool relative".into(),
        cwd: dir.path().to_path_buf(),
        env: vec![
            ("PATH".into(), path),
            ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD".into()),
            ("SystemRoot".into(), system_root.clone()),
            (
                "ComSpec".into(),
                format!("{system_root}\\System32\\cmd.exe"),
            ),
        ],
        output: writer,
    })
    .unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    while !run.exited() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run.code(), Some(0), "{out}");
    assert_eq!(out.replace("\r\n", "\n"), "cmd bare\ncmd relative\n");
}

#[cfg(unix)]
#[test]
fn cleanup_failures_are_not_empty_process_lists() {
    let deadline = Instant::now() + Duration::from_secs(60);
    for failure in ["discovery", "environment", "kill", "retry limit"] {
        let mut budget = 100;
        let result = cleanup_groups(
            "token",
            deadline,
            &mut budget,
            |_, _| {
                if failure == "discovery" {
                    Err("discovery".into())
                } else {
                    Ok(vec![123])
                }
            },
            |leaders, _, _, _| {
                if failure == "environment" {
                    Err("environment".into())
                } else {
                    Ok(leaders.to_vec())
                }
            },
            |_| {
                if failure == "kill" {
                    Err(std::io::Error::other("kill"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err(), "{failure}");
    }
    assert!(
        cleanup_groups(
            "token",
            deadline,
            &mut 100,
            |_, _| Ok(vec![123]),
            |_, _, _, _| Ok(Vec::new()),
            |_| panic!("unmarked group killed")
        )
        .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn discovery_has_shared_deadline_and_output_budget() {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut budget = 10;
    let mut calls = 0;
    let result = cleanup_groups(
        "token",
        deadline,
        &mut budget,
        |observed, remaining| {
            assert_eq!(observed, deadline);
            calls += 1;
            assert_eq!(*remaining, if calls == 1 { 10 } else { 6 });
            *remaining -= 1;
            Ok(if calls == 1 { vec![123] } else { vec![] })
        },
        |leaders, _, observed, remaining| {
            assert_eq!(observed, deadline);
            assert_eq!(leaders, [123]);
            *remaining -= 3;
            Ok(leaders.to_vec())
        },
        |_| Ok(()),
    );
    assert!(result.is_ok());
    assert_eq!(budget, 5);
    let expired = Instant::now() - Duration::from_secs(1);
    assert!(
        cleanup_groups(
            "token",
            expired,
            &mut 10,
            |_, _| panic!("expired discovery ran"),
            |_, _, _, _| unreachable!(),
            |_| unreachable!()
        )
        .is_err()
    );
    assert!(
        discovery_output(std::process::Command::new("must-not-run"), deadline, &mut 0).is_err()
    );
    assert!(
        discovery_output(std::process::Command::new("must-not-run"), expired, &mut 10).is_err()
    );
}
