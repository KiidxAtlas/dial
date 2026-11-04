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
            |_, _, _, _| {
                if failure == "environment" {
                    Err("environment".into())
                } else {
                    Ok(true)
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
            |_, _, _, _| Ok(false),
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
        |_, _, observed, remaining| {
            assert_eq!(observed, deadline);
            *remaining -= 3;
            Ok(true)
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
