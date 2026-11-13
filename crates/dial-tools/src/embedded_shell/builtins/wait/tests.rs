use crate::embedded_shell::builtins::tests::run_script;

/// Runs `script` with stdout and stderr captured to files; returns `(status, stdout, stderr)`.
fn run(script: &str) -> (u8, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let code = run_script(dir.path(), &format!("{{\n{script}\n}} >out 2>err"));
    let read = |name| std::fs::read_to_string(dir.path().join(name)).unwrap_or_default();
    (code, read("out"), read("err"))
}

#[cfg(unix)]
#[test]
fn wait_for_a_pid_returns_the_job_status() {
    let (code, out, err) = run(
        "sh -c 'exit 7' & p=$!\nwait \"$p\"; echo \"status=$?\"\nwait \"$p\"; echo \"again=$?\"",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "status=7\nagain=127\n");
    assert!(err.contains("is not a child of this shell"), "{err}");
}

#[cfg(unix)]
#[test]
fn wait_for_a_killed_pid_reports_the_signal() {
    let (_, out, err) = run("sleep 30 & p=$!\nkill \"$p\"; wait \"$p\"; echo \"status=$?\"");
    assert_eq!(out, "status=143\n", "{err}");
}

#[test]
fn wait_for_a_job_spec_returns_its_status() {
    let (_, out, err) =
        run("(exit 3) &\nwait %1; echo \"status=$?\"\nwait %9; echo \"missing=$?\"");
    assert_eq!(out, "status=3\nmissing=127\n", "{err}");
}

#[test]
fn wait_without_operands_waits_for_every_job() {
    let (code, out, err) = run("(echo one) &\n(echo two) &\nwait; echo \"done=$?\"");
    assert_eq!(code, 0, "{err}");
    let mut lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.pop(), Some("done=0"));
    lines.sort_unstable();
    assert_eq!(lines, ["one", "two"]);
}

#[test]
fn wait_next_returns_the_status_of_the_first_job_to_finish() {
    // `sleep` keeps job 1 running, so job 2 is the one `wait -n` returns for.
    let (code, out, err) = run(
        "sleep 30 &\n(exit 5) &\nwait -n; echo \"status=$?\"\nkill %1; wait %1; echo \"killed=$?\"",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "status=5\nkilled=143\n");
}

#[test]
fn wait_next_without_jobs_returns_127() {
    let (_, out, err) = run("wait -n; echo \"status=$?\"");
    assert_eq!(out, "status=127\n", "{err}");
}

#[test]
fn wait_p_stores_the_pid_of_the_job_waited_for() {
    let (code, out, err) = run(
        "(exit 6) & p=$!\nwait -p got %1; echo \"status=$?\"\n[ \"$got\" = \"$p\" ] && echo same",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "status=6\nsame\n");
}

#[test]
fn wait_options_combine_and_bad_ones_are_usage_errors() {
    let (code, out, err) = run("(exit 2) &\nwait -fn; echo \"status=$?\"");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "status=2\n");
    let cases = [
        ("wait -x", 2),
        ("wait -p", 2),
        ("wait -p 1x", 1),
        ("wait abc", 2),
        ("wait -- -1", 2),
    ];
    for (script, expected) in cases {
        assert_eq!(run(script).0, expected, "script: {script}");
    }
}

#[test]
fn wait_finds_a_job_that_started_no_process_by_its_pid() {
    // `(exit 4) &` starts no external process; `$!` is still a PID, a virtual one, and `wait`
    // finds the job by it.
    let (code, out, err) = run("(exit 4) & p=$!\n[ -n \"$p\" ] && wait \"$p\"; echo \"status=$?\"");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "status=4\n");
}

#[cfg(unix)]
#[test]
fn kill_and_wait_report_the_signal_for_each_way_to_name_a_job() {
    // Each script stops a `sleep` (or a pipeline holding one) and waits for the job: by `$!` of a
    // braced job, by `%1` straight after `&` (before its process starts), and by `$!` of a
    // pipeline whose last member is an external process or a builtin.
    let cases = [
        "{ sleep 30; } & p=$!; [ -n \"$p\" ]; kill $p; wait $p; echo \"status=$?\"",
        "sleep 30 & kill %1; wait %1; echo \"status=$?\"",
        "sleep 30 | cat & p=$!; [ -n \"$p\" ]; kill $p; wait $p; echo \"status=$?\"",
        "sleep 30 | true & p=$!; kill $p; wait $p; echo \"status=$?\"",
    ];
    for script in cases {
        let (code, out, err) = run(script);
        assert_eq!(code, 0, "{script}: {err}");
        assert_eq!(out, "status=143\n", "{script}");
    }
}

#[test]
fn wait_rejects_bad_operands() {
    assert_eq!(run("wait abc").0, 2);
}

#[test]
fn kill_stops_a_background_loop_of_builtins() {
    // The loop never awaits I/O; brush's loops yield cooperatively so the kill is observed.
    for script in [
        "while :; do :; done & p=$!\nkill \"$p\"; wait \"$p\"; echo \"status=$?\"",
        "for ((i = 0; 1; i++)); do :; done &\nkill %1; wait %1; echo \"status=$?\"",
    ] {
        let (code, out, err) = run(script);
        assert_eq!(code, 0, "{script}: {err}");
        assert_eq!(out, "status=143\n", "{script}");
    }
}
