use super::*;
#[cfg(target_os = "macos")]
use crate::tools::execute;
use crate::tools::testkit::*;
use serde_json::json;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
fn sandboxed() -> bool {
    super::super::sandbox::require_for_test()
}

fn bash(ws: &Workspace, command: &str) -> ToolOutput {
    run(ws, "bash", json!({"command": command}))
}

#[test]
fn cwd_argument_and_leading_cd_are_confined_to_workspace() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("src")).unwrap();
    let (cwd, command) = working_directory(&ws, &json!({"cwd": "src"}), "pwd").unwrap();
    assert_eq!(cwd, ws.root().join("src"));
    assert_eq!(command, "pwd");
    let (cwd, command) = working_directory(&ws, &json!({}), "cd src && pwd").unwrap();
    assert_eq!(cwd, ws.root().join("src"));
    assert_eq!(command, "pwd");
    assert!(working_directory(&ws, &json!({"cwd": "../"}), "pwd").is_err());
}

#[test]
fn cwd_may_be_an_added_root_but_not_an_undeclared_directory_or_a_file() {
    let (dir, mut ws) = workspace();
    let added = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(added.path().join("pkg")).unwrap();
    fs::write(added.path().join("file.txt"), "x").unwrap();
    let added_root = added.path().canonicalize().unwrap();
    let added_text = added_root.to_string_lossy().into_owned();
    // Not yet added: refused.
    assert!(working_directory(&ws, &json!({"cwd": added_text}), "pwd").is_err());
    ws.add_root(&added_root).unwrap();
    let (cwd, _) = working_directory(&ws, &json!({"cwd": added_text}), "pwd").unwrap();
    assert_eq!(cwd, added_root);
    let (cwd, command) =
        working_directory(&ws, &json!({}), &format!("cd {added_text}/pkg && pwd")).unwrap();
    assert_eq!((cwd, command.as_str()), (added_root.join("pkg"), "pwd"));
    // A leading relative `cd` resolves against the explicit added-root cwd.
    let (cwd, _) = working_directory(&ws, &json!({"cwd": added_text}), "cd pkg && ls").unwrap();
    assert_eq!(cwd, added_root.join("pkg"));
    for denied in [
        outside.path().to_string_lossy().into_owned(),
        format!("{added_text}/file.txt"),
        format!("{added_text}/missing"),
        format!("{added_text}/../"),
    ] {
        assert!(
            working_directory(&ws, &json!({"cwd": denied}), "pwd").is_err(),
            "{denied}"
        );
    }
    assert!(working_directory(&ws, &json!({"cwd": "."}), "pwd").is_ok());
    drop(dir);
}
#[cfg(unix)]
#[test]
fn steering_hands_off_running_work_without_killing_its_effects() {
    let (dir, ws) = workspace();
    let ws = ws.without_sandbox();
    let signals = super::super::signals::CallSignals::new();
    signals.request_steering();
    let adopted = parking_lot::Mutex::new(None);
    let output = super::super::signals::scope(&signals, || {
        bash_tool_streaming_with_adopter(
            &ws,
            &json!({"command":"sleep 0.1; printf survived > result.txt"}),
            5,
            0,
            &|_| {},
            &AtomicBool::new(false),
            &|job| {
                *adopted.lock() = Some(job);
                Ok("bg-test".into())
            },
        )
    })
    .unwrap();
    assert!(output.text.contains("bg-test"));
    let mut job = adopted.lock().take().unwrap();
    assert!(job.child.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "survived"
    );
}

#[cfg(unix)]
#[test]
fn explicit_background_and_threshold_both_adopt_with_a_notice() {
    let (_dir, ws) = workspace();
    let ws = ws.without_sandbox();
    let cancel = AtomicBool::new(false);
    let adopted = parking_lot::Mutex::new(0usize);
    let on_adopt = |_job: ForegroundJob| {
        *adopted.lock() += 1;
        Ok("bg7".to_owned())
    };
    let explicit = bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 5", "background": true}),
        0,
        0,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(
        explicit.text.contains("background job bg7"),
        "{}",
        explicit.text
    );
    let threshold = bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 5"}),
        0,
        1,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(
        threshold.text.contains("background job bg7"),
        "{}",
        threshold.text
    );
    assert_eq!(*adopted.lock(), 2);
}

#[cfg(unix)]
#[test]
fn an_explicit_timeout_does_not_keep_a_long_command_in_the_foreground() {
    let (_dir, ws) = workspace();
    let ws = ws.without_sandbox();
    let cancel = AtomicBool::new(false);
    let adopted = parking_lot::Mutex::new(0usize);
    let on_adopt = |_job: ForegroundJob| {
        *adopted.lock() += 1;
        Ok("bg7".to_owned())
    };
    // Finishes inside the 1s threshold: the result returns in this one call.
    let finished = bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 0.2; echo built", "timeout": 10}),
        0,
        1,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(
        finished.text.contains("built") && !finished.is_error,
        "{}",
        finished.text
    );
    assert_eq!(*adopted.lock(), 0);
    // Still running past the 1s threshold though its timeout is 10s: moved to bg at the threshold.
    let long = bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 5", "timeout": 10}),
        0,
        1,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(long.text.contains("background job bg7"), "{}", long.text);
    assert_eq!(*adopted.lock(), 1);
    // A timeout shorter than the threshold still kills the group rather than adopting it.
    let killed = bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 5", "timeout": 1}),
        0,
        5,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(killed.text.contains("timed out"), "{}", killed.text);
    assert_eq!(*adopted.lock(), 1);
}
#[cfg(unix)]
#[test]
fn hard_cancel_with_steering_kills_the_command_instead_of_adopting_it() {
    let (dir, ws) = workspace();
    let ws = ws.without_sandbox();
    let signals = super::super::signals::CallSignals::new();
    let cancel = AtomicBool::new(false);
    let adopted = AtomicBool::new(false);
    // The first output arrives while nothing is pending; then both triggers become true
    // before the next pass, which must choose the hard cancel.
    let on_output = |_: &[u8]| {
        signals.request_steering();
        cancel.store(true, Ordering::SeqCst);
    };
    let began = Instant::now();
    let output = super::super::signals::scope(&signals, || {
        bash_tool_streaming_with_adopter(
            &ws,
            &json!({"command": "echo started; sleep 5; printf survived > result.txt"}),
            0,
            0,
            &on_output,
            &cancel,
            &|_job| {
                adopted.store(true, Ordering::SeqCst);
                Ok("bg-unexpected".into())
            },
        )
    })
    .unwrap();
    assert!(!adopted.load(Ordering::SeqCst), "{}", output.text);
    assert!(
        output.text.contains("[cancelled; process group killed]"),
        "{}",
        output.text
    );
    assert!(output.is_error);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "cancel waited for the sleep"
    );
    // The owner killed the group, so the command never reached its final write.
    assert!(!dir.path().join("result.txt").exists());
}

#[test]
fn named_sessions_get_the_same_adoption_deadline() {
    let threshold = Duration::from_secs(5);
    assert_eq!(adoption_deadline(threshold), Some(threshold));
    assert_eq!(adoption_deadline(Duration::ZERO), None);
}

#[test]
fn an_explicit_timeout_never_postpones_adoption() {
    let threshold = Duration::from_secs(15);
    let long = Duration::from_secs(600);
    // The threshold is the deadline whatever the timeout; a long timeout does not move it.
    assert_eq!(adoption_deadline(threshold), Some(threshold));
    // A timeout at or after the deadline never kills: adoption happens first.
    assert!(!timeout_kills(long, long, Some(threshold)));
    assert!(!timeout_kills(threshold, threshold, Some(threshold)));
    // A timeout shorter than the threshold still kills, and so does any timeout without backgrounding.
    let short = Duration::from_secs(5);
    assert!(timeout_kills(short, short, Some(threshold)));
    assert!(timeout_kills(short, short, None));
    assert!(!timeout_kills(Duration::ZERO, long, None));
}

#[test]
fn adoption_is_due_only_for_the_policy_triggers() {
    let deadline = Some(Duration::from_secs(5));
    assert!(!adoption_due(
        false,
        false,
        Duration::from_secs(4),
        deadline
    ));
    assert!(adoption_due(false, false, Duration::from_secs(5), deadline));
    // Explicit background and steering adopt at once, whatever the threshold.
    assert!(adoption_due(true, false, Duration::ZERO, None));
    assert!(adoption_due(false, true, Duration::ZERO, None));
    assert!(!adoption_due(false, false, Duration::from_secs(600), None));
}

#[test]
fn hard_cancel_outranks_every_adoption_trigger() {
    assert!(!may_adopt(true, true));
    assert!(!may_adopt(true, false));
    assert!(may_adopt(false, true));
    assert!(!may_adopt(false, false));
}
#[cfg(unix)]
#[test]
fn preview_servers_are_redirected_before_spawn_but_finite_commands_still_run() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, ws) = workspace();
    let ws = ws.without_sandbox();
    fs::write(
        dir.path().join("pnpm"),
        "#!/bin/sh\necho launched > launched.txt\n",
    )
    .unwrap();
    fs::set_permissions(dir.path().join("pnpm"), fs::Permissions::from_mode(0o755)).unwrap();
    let refused = bash(&ws, "./pnpm dev --host 127.0.0.1 --port 3000");
    assert!(
        refused.is_error
            && refused.text.contains("Command not started")
            && refused.text.contains("bg"),
        "{}",
        refused.text
    );
    assert!(
        !dir.path().join("launched.txt").exists(),
        "no unmanaged process may start"
    );
    let build = bash(&ws, "./pnpm run build");
    assert!(
        !build.is_error && build.text.contains("[exit code: 0]"),
        "{}",
        build.text
    );
    assert!(dir.path().join("launched.txt").exists());
    for command in [
        "npm run dev",
        "pnpm dev",
        "yarn dev",
        "bun run dev",
        "nuxt dev",
        "nuxi dev",
        "next dev",
        "astro dev",
        "vite --host 127.0.0.1",
        "python3 -m http.server 3000",
    ] {
        assert!(preview_server(command), "{command}");
    }
    for command in [
        "pnpm dev --help",
        "pnpm watch --once",
        "vite --version",
        "vite build",
        "pnpm run build",
        "npm test",
        "cargo test",
        "printf 'pnpm dev'",
        "echo ok; pnpm dev",
    ] {
        assert!(!preview_server(command), "{command}");
    }
}

#[test]
fn bash_refuses_without_sandbox() {
    let (dir, ws) = workspace();
    let ws = ws.with_sandbox(Sandbox::Unavailable("test says no".to_string()));
    let out = bash(&ws, "touch ran.txt");
    assert!(
        out.is_error && out.text.contains("test says no"),
        "{}",
        out.text
    );
    assert!(!dir.path().join("ran.txt").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn bash_reads_and_writes_inside_workspace_only() {
    if !sandboxed() {
        return;
    }
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    let canary = outside.path().join("canary");
    fs::write(&canary, "OUTSIDE-SECRET").unwrap();
    let out = bash(
        &ws,
        "echo inside > made.txt && cat made.txt && mkdir -p d/e && echo x > d/e/f && pwd",
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("inside"));
    assert!(
        out.text
            .contains(dir.path().canonicalize().unwrap().to_str().unwrap())
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("made.txt")).unwrap(),
        "inside\n"
    );

    let read = bash(&ws, &format!("cat '{}'", canary.display()));
    assert!(
        read.is_error && !read.text.contains("OUTSIDE-SECRET"),
        "{}",
        read.text
    );
    let write = bash(
        &ws,
        &format!("echo pwn > '{}/new'", outside.path().display()),
    );
    assert!(write.is_error && !outside.path().join("new").exists());
    let stat = bash(&ws, &format!("ls '{}'", outside.path().display()));
    assert!(stat.is_error);
    // Redirections to /dev/null and heredocs (temp files) work.
    let misc = bash(&ws, "echo hidden >/dev/null 2>&1; cat <<EOF\nhere\nEOF");
    assert!(
        !misc.is_error && misc.text.contains("here"),
        "{}",
        misc.text
    );
}

#[cfg(target_os = "macos")]
#[test]
fn bash_cannot_read_home() {
    if !sandboxed() {
        return;
    }
    let (_dir, ws) = workspace();
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let mut canary = tempfile::Builder::new()
        .prefix(".dial-sandbox-canary-")
        .tempfile_in(&home)
        .unwrap();
    io::Write::write_all(&mut canary, b"HOME-SECRET").unwrap();
    assert_eq!(fs::read_to_string(canary.path()).unwrap(), "HOME-SECRET");
    let out = bash(&ws, &format!("cat '{}'", canary.path().display()));
    assert!(
        out.is_error && !out.text.contains("HOME-SECRET"),
        "{}",
        out.text
    );
    let listing = bash(&ws, &format!("ls -a '{}'", home.display()));
    assert!(listing.is_error, "{}", listing.text);
    // The scrubbed HOME is the workspace, not the real home.
    let env = bash(&ws, "echo $HOME");
    assert!(
        env.text.starts_with(ws.root().to_str().unwrap()),
        "{}",
        env.text
    );
}

#[cfg(target_os = "macos")]
#[test]
fn bash_environment_is_scrubbed() {
    if !sandboxed() {
        return;
    }
    let (_dir, ws) = workspace();
    // cargo exports CARGO_* variables into this test process.
    assert!(std::env::vars().any(|(k, _)| k.starts_with("CARGO")));
    let out = bash(&ws, "env | sort");
    assert!(!out.is_error);
    for line in out.text.lines() {
        let key = line.split('=').next().unwrap_or("");
        assert!(
            [
                "PATH",
                "HOME",
                "TMPDIR",
                "PWD",
                "SHELL",
                "LANG",
                "TERM",
                "DEVELOPER_DIR",
                "SHLVL",
                "_",
                "OLDPWD",
                // Non-interactive git/credential defaults set by `shell_command`.
                "GIT_TERMINAL_PROMPT",
                "GCM_INTERACTIVE",
                "GIT_ASKPASS",
                "SSH_ASKPASS"
            ]
            .contains(&key)
                || line.starts_with('['),
            "unexpected environment entry {line:?}"
        );
    }
    assert!(out.text.contains("TMPDIR="));
}

#[cfg(target_os = "macos")]
#[test]
fn bash_network_is_blocked_unless_allowed() {
    if !sandboxed() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
    // Control: the same command outside the sandbox does connect.
    let control = std::process::Command::new("/bin/bash")
        .args(["-c", &probe])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&control.stdout).contains("CONNECTED"),
        "control failed"
    );

    let (_dir, ws) = workspace();
    let blocked = bash(&ws, &probe);
    assert!(
        blocked.is_error && !blocked.text.contains("CONNECTED"),
        "{}",
        blocked.text
    );
    let curl = bash(&ws, "/usr/bin/curl --max-time 3 -sS https://example.com");
    assert!(
        curl.is_error && !curl.text.contains("Example Domain"),
        "{}",
        curl.text
    );

    let (_dir2, open) = workspace();
    let open = open.with_network(true);
    let allowed = bash(&open, &probe);
    assert!(allowed.text.contains("CONNECTED"), "{}", allowed.text);
}

#[cfg(target_os = "macos")]
#[test]
fn bash_symlink_escape_is_blocked() {
    if !sandboxed() {
        return;
    }
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("canary"), "LINKED-SECRET").unwrap();
    let out = bash(
        &ws,
        &format!(
            "ln -s '{0}/canary' flink && ln -s '{0}' dlink; cat flink; cat dlink/canary; echo pwn > dlink/new",
            outside.path().display()
        ),
    );
    assert!(
        out.is_error && !out.text.contains("LINKED-SECRET"),
        "{}",
        out.text
    );
    assert!(!outside.path().join("new").exists());
    // The symlinks now exist in the workspace; the fs tools refuse them too.
    assert!(dir.path().join("flink").symlink_metadata().is_ok());
    assert!(run(&ws, "read", json!({"path": "flink"})).is_error);
    assert!(run(&ws, "read", json!({"path": "dlink/canary"})).is_error);
    assert!(run(&ws, "write", json!({"path": "dlink/new", "content": "x"})).is_error);
    assert!(
        !run(&ws, "grep", json!({"pattern": "LINKED"}))
            .text
            .contains("LINKED-SECRET")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn bash_output_is_bounded_and_keeps_head_and_tail() {
    if !sandboxed() {
        return;
    }
    let (_dir, ws) = workspace();
    let out = bash(&ws, "echo START; seq 1 300000; echo END");
    assert!(!out.is_error);
    assert!(
        out.text.len() < BASH_HEAD + BASH_TAIL + 200,
        "{}",
        out.text.len()
    );
    assert!(out.text.starts_with("START\n") && out.text.contains("bytes of output omitted"));
    assert!(out.text.contains("\nEND\n[exit code: 0]"));
    let flood = bash(&ws, "yes | head -c 20000000");
    assert!(flood.text.len() < BASH_HEAD + BASH_TAIL + 200);
    let failing = bash(&ws, "echo oops >&2; exit 3");
    assert!(
        failing.is_error
            && failing.text.contains("oops")
            && failing.text.contains("[exit code: 3]")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn bash_keeps_diagnostics_from_the_omitted_middle_within_bounds() {
    if !sandboxed() {
        return;
    }
    let (_dir, ws) = workspace();
    let out = bash(
        &ws,
        "seq 1 50000; echo 'error[E0308]: mismatched types'; echo '  --> src/lib.rs:7:5'; seq 1 50000; exit 101",
    );
    assert!(out.is_error);
    let omitted = out
        .text
        .find("diagnostic line(s) from that part follow")
        .expect("marker");
    let error = out
        .text
        .find("error[E0308]: mismatched types\n  --> src/lib.rs:7:5\n")
        .expect("the middle error survives");
    assert!(
        omitted < error && out.text.contains("[exit code: 101]"),
        "{}",
        &out.text[omitted..omitted + 300]
    );
    assert!(
        out.text.len() < BASH_HEAD + BASH_TAIL + BASH_DIAGNOSTICS + 300,
        "{}",
        out.text.len()
    );
}

#[cfg(target_os = "macos")]
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(target_os = "macos")]
fn read_pid(dir: &Path, name: &str) -> i32 {
    fs::read_to_string(dir.join(name))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[cfg(target_os = "macos")]
#[test]
fn bash_timeout_kills_process_group_without_orphans() {
    if !sandboxed() {
        return;
    }
    let (dir, ws) = workspace();
    let started = Instant::now();
    let out = run(
        &ws,
        "bash",
        json!({"command": "sleep 30 & echo $! > bg.pid; echo $$ > sh.pid; echo partial; sleep 30", "timeout": 1}),
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        out.is_error && out.text.contains("timed out after 1s") && out.text.contains("partial"),
        "{}",
        out.text
    );
    assert_eq!(
        out.execution.status,
        Some(crate::tools::ToolStatus::Timeout)
    );
    assert_eq!(
        out.execution.effects,
        crate::tools::surface::outcome::Effects::Known,
        "a reaped timeout is an ordinary failure, not an uncertain effect that ends the turn"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !alive(read_pid(dir.path(), "bg.pid")),
        "backgrounded child survived"
    );
    assert!(!alive(read_pid(dir.path(), "sh.pid")), "shell survived");
}

#[cfg(target_os = "macos")]
#[test]
fn bash_kills_background_jobs_when_command_returns() {
    if !sandboxed() {
        return;
    }
    let (dir, ws) = workspace();
    let started = Instant::now();
    let out = bash(&ws, "sleep 30 & echo $! > bg.pid; echo done");
    assert!(!out.is_error && out.text.contains("done"), "{}", out.text);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !alive(read_pid(dir.path(), "bg.pid")),
        "background job outlived the call"
    );
    // The harness cap wins over a larger requested timeout.
    let capped = execute(
        &ws,
        "bash",
        &json!({"command": "sleep 30", "timeout": 600}),
        1,
    );
    assert!(
        capped.text.contains("timed out after 1s"),
        "{}",
        capped.text
    );
    assert!(bash(&ws, "   ").is_error);
}

#[cfg(target_os = "macos")]
#[test]
fn bash_runs_python_and_git_when_homebrew_is_present() {
    if !sandboxed() {
        return;
    }
    let (_dir, ws) = workspace();
    let out = bash(
        &ws,
        "python3 -c 'import json, sys; print(json.dumps([6*7]))'",
    );
    assert!(
        !out.is_error && out.text.contains("[42]"),
        "python3: {}",
        out.text
    );
    let git = bash(
        &ws,
        "git init -q repo && cd repo && echo a > a && git add a && \
             git -c user.name=t -c user.email=t@example.com commit -q -m first && git log --oneline | wc -l",
    );
    assert!(
        !git.is_error && git.text.trim_start().starts_with('1'),
        "git: {}",
        git.text
    );
}

#[test]
fn streaming_output_precedes_result_and_has_bounded_chunks() {
    let (dir, ws) = workspace();
    let ws = ws.without_sandbox();
    let events = parking_lot::Mutex::new(Vec::new());
    let on_output = |bytes: &[u8]| {
        assert!(bytes.len() <= 16 * 1024);
        assert!(
            !dir.path().join("finished").exists(),
            "output arrived only after command finished"
        );
        events
            .lock()
            .push(String::from_utf8_lossy(bytes).into_owned());
    };
    let output = bash_tool_streaming(
        &ws,
        &json!({
            "command": "printf 'early\\n'; sleep 0.3; touch finished"
        }),
        5,
        &on_output,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!output.is_error, "{}", output.text);
    let mut events = events.into_inner();
    assert!(events.join("").contains("early\n"));
    events.push("RESULT".into());
    assert!(events.first().unwrap().contains("early"));
    assert_eq!(events.last().unwrap(), "RESULT");
    assert!(dir.path().join("finished").exists());
}

#[test]
fn complete_artifact_keeps_middle_of_large_binary_output() {
    let (_dir, ws) = workspace();
    let artifacts = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
    let ws = ws.without_sandbox().with_artifacts(store);
    let output = bash_tool_streaming(&ws, &json!({
        "command": "printf 'BEGIN\\n'; head -c 1048576 /dev/zero; printf '\\nMIDDLE\\n'; head -c 1048576 /dev/zero; printf '\\nEND\\n'"
    }), 10, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert!(!output.text.contains("MIDDLE"));
    let uri = output
        .text
        .lines()
        .find_map(|line| line.strip_prefix("[complete output: "))
        .unwrap()
        .trim_end_matches(']');
    let bytes = fs::read(
        artifacts
            .path()
            .join(uri.strip_prefix("artifact://").unwrap()),
    )
    .unwrap();
    assert_eq!(bytes.len(), 2 * 1048576 + b"BEGIN\n\nMIDDLE\n\nEND\n".len());
    assert!(bytes.starts_with(b"BEGIN\n") && bytes.ends_with(b"\nEND\n"));
    assert_eq!(&bytes[1048576 + 6..1048576 + 14], b"\nMIDDLE\n");
    assert!(output.text.len() < BASH_HEAD + BASH_TAIL + 400);
}

static SIGNAL_CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel_on_sigint(_: libc::c_int) {
    SIGNAL_CANCEL.store(true, Ordering::Release);
    // Lock-free and async-signal-safe: wakes the waiting shell loop immediately.
    crate::platform::wake::signal();
}

// Run in its own test process: installing SIGINT must not affect concurrent tests.
#[cfg(unix)]
#[test]
#[ignore]
fn sigint_shell_helper() {
    let root =
        std::env::var("DIAL_TEST_SHELL_SIGINT_ROOT").expect("launched by SIGINT regression test");
    let ws = Workspace::new(Path::new(&root)).unwrap().without_sandbox();
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = cancel_on_sigint as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    let output = bash_tool_streaming(
        &ws,
        &json!({
            "command": "sleep 30 & echo $! > bg.pid; echo $$ > sh.pid; echo READY; wait"
        }),
        30,
        &|bytes| {
            if !bytes.is_empty() {
                fs::write(ws.root().join("ready"), b"ready").unwrap();
            }
        },
        &SIGNAL_CANCEL,
    )
    .unwrap();
    assert!(
        output.is_error && output.text.contains("cancelled"),
        "{}",
        output.text
    );
}

#[cfg(unix)]
#[test]
fn sigint_cancellation_kills_shell_and_process_group_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::exec::shell::tests::sigint_shell_helper",
            "--ignored",
        ])
        .env("DIAL_TEST_SHELL_SIGINT_ROOT", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("ready").exists() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("SIGINT helper never produced live output");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = Instant::now();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("SIGINT did not promptly cancel the shell");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "SIGINT helper failed: {status}");
    for name in ["sh.pid", "bg.pid"] {
        let pid: i32 = fs::read_to_string(dir.path().join(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "{name} survived cancellation"
        );
    }
}

fn with_artifacts(minimize: bool) -> (tempfile::TempDir, Workspace, tempfile::TempDir) {
    let (dir, ws) = workspace();
    let artifacts = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
    let shell = crate::config::types::ShellConfig {
        minimize_output: minimize,
        ..Default::default()
    };
    let ws = ws
        .without_sandbox()
        .with_artifacts(store)
        .with_shell_config(&shell);
    (dir, ws, artifacts)
}

/// A fake `cargo` that prints build progress, so the filter is chosen by the command name alone.
fn fake_cargo(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = "#!/bin/sh\nfor i in $(seq 1 80); do echo \"   Compiling crate$i v1.0.$i\"; done\necho 'warning: unused variable'\necho 'error: boom' >&2\nexit 101\n";
    let path = dir.join("cargo");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn minimized_output_keeps_status_diagnostics_and_a_raw_artifact() {
    let (dir, ws, artifacts) = with_artifacts(true);
    fake_cargo(dir.path());
    let command = "PATH=.:$PATH cargo build";
    let output = bash(&ws, command);
    assert!(output.is_error, "{}", output.text);
    assert!(
        !output.text.contains("Compiling crate7 "),
        "{}",
        output.text
    );
    for kept in [
        "warning: unused variable",
        "error: boom",
        "[exit code: 101]",
        "[output filtered (cargo): ",
    ] {
        assert!(output.text.contains(kept), "{kept}: {}", output.text);
    }
    let uri = output
        .text
        .lines()
        .find_map(|line| line.strip_prefix("[complete output: "))
        .expect("raw output artifact")
        .trim_end_matches(']');
    let raw = fs::read_to_string(
        artifacts
            .path()
            .join(uri.strip_prefix("artifact://").unwrap()),
    )
    .unwrap();
    assert!(raw.contains("Compiling crate7 v1.0.7") && raw.contains("error: boom"));
}

#[cfg(unix)]
#[test]
fn minimizing_can_be_turned_off_and_needs_an_artifact() {
    let (dir, ws, _artifacts) = with_artifacts(false);
    fake_cargo(dir.path());
    let output = bash(&ws, "PATH=.:$PATH cargo build");
    assert!(
        output.text.contains("Compiling crate7 v1.0.7"),
        "{}",
        output.text
    );
    assert!(!output.text.contains("[output filtered"));

    // Without an artifact store the raw output would be lost, so nothing is filtered.
    let (dir, ws) = workspace();
    fake_cargo(dir.path());
    let shell = crate::config::types::ShellConfig::default();
    let ws = ws.without_sandbox().with_shell_config(&shell);
    let output = bash(&ws, "PATH=.:$PATH cargo build");
    assert!(
        output.text.contains("Compiling crate7 v1.0.7"),
        "{}",
        output.text
    );
}

#[cfg(unix)]
#[test]
fn compound_commands_are_not_minimized() {
    let (dir, ws, _artifacts) = with_artifacts(true);
    fake_cargo(dir.path());
    let output = bash(&ws, "PATH=.:$PATH cargo build; true");
    assert!(
        output.text.contains("Compiling crate7 v1.0.7"),
        "{}",
        output.text
    );
}

#[test]
fn the_git_guard_refuses_before_anything_runs() {
    let (dir, ws) = workspace();
    let ws = ws.without_sandbox().with_shared_checkout(true);
    fs::write(dir.path().join("keep.txt"), "x").unwrap();
    let output = bash(&ws, "touch ran.txt && git clean -fd");
    assert!(output.is_error);
    assert!(output.text.contains("was not run"), "{}", output.text);
    assert!(!dir.path().join("ran.txt").exists());
    assert!(dir.path().join("keep.txt").exists());
    assert_eq!(
        crate::tools::ToolStatus::from_output(&output),
        crate::tools::ToolStatus::Blocked
    );
    // Not a shared checkout: the default policy leaves the command alone.
    let (_dir, plain) = workspace();
    let output = bash(&plain.without_sandbox(), "git clean -fd");
    assert!(!output.text.contains("was not run"), "{}", output.text);
}

#[test]
fn artifact_cap_is_reported_separately_from_capture() {
    use crate::runtime::execution::Completeness;
    let (_dir, ws) = workspace();
    let storage = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(storage.path())
        .unwrap()
        .with_limits(&crate::storage::artifact::ArtifactConfig {
            max_bytes: 8,
            head_bytes: 4,
        });
    let ws = ws.without_sandbox().with_artifacts(store);
    let out = bash(&ws, "printf 0123456789abcdef");
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("[partial output: artifact://"),
        "{}",
        out.text
    );
    let integrity = out.execution.output.unwrap();
    assert_eq!(integrity.capture, Completeness::Complete);
    assert_eq!(integrity.retention, Completeness::Partial);
    assert_eq!(integrity.presentation, Completeness::Complete);
    assert_eq!(integrity.original_bytes, Some(16));
    assert_eq!(integrity.retained_bytes, Some(8));
    assert_eq!(integrity.omitted_bytes, Some(8));
}
