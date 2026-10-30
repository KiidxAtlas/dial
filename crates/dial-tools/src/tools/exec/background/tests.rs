use super::table::{MAX_BUFFER, MAX_PROCESSES, Ring};
use super::*;
#[cfg(target_os = "macos")]
use crate::tools::exec::sandbox::Policy;
use crate::tools::exec::sandbox::Sandbox;
use crate::tools::testkit::workspace;
use std::fs;
#[cfg(target_os = "macos")]
use std::path::Path;

/// A workspace whose commands run unsandboxed (scrubbed env only), so these tests do not depend on
/// `sandbox-exec`; the sandbox tests below opt in explicitly.
fn open_workspace() -> (tempfile::TempDir, Workspace) {
    let (dir, ws) = workspace();
    (dir, ws.without_sandbox())
}

fn bg(tool: &Background, ws: &Workspace, args: Value) -> ToolOutput {
    let config = Config::default();
    tool.call(
        &ToolContext {
            workspace: ws,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    )
}

fn tool() -> Background {
    Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: true,
    }
}

#[test]
fn default_wait_returns_control_while_the_job_keeps_running() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    assert!(!start(&tool, &ws, "bounded", "sleep 30").is_error);
    let began = Instant::now();
    let out = bg(&tool, &ws, json!({"action":"wait", "name":"bounded"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("still running"), "{}", out.text);
    assert!(began.elapsed() < Duration::from_secs(15));
    assert!(tool.has_live_work());
    assert!(!bg(&tool, &ws, json!({"action":"kill", "name":"bounded"})).is_error);
}

#[test]
fn waits_release_the_workspace_lock_and_readiness_is_steerable() {
    let tool = tool();
    for action in ["wait", "output", "list"] {
        assert!(matches!(
            tool.concurrency(&json!({"action":action})),
            crate::tools::integrations::ext::Concurrency::Shared
        ));
    }
    assert!(matches!(
        tool.concurrency(&json!({"action":"start"})),
        crate::tools::integrations::ext::Concurrency::Exclusive
    ));
    assert!(tool.interruptible(&json!({"action":"start", "ready":{"log":"ready"}})));
    assert!(!tool.interruptible(&json!({"action":"start"})));
}

#[test]
fn pty_notifications_use_job_identity_and_replace_semantics() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    let out = bg(
        &tool,
        &ws,
        json!({"action":"start", "name":"terminal", "pty":true, "command":"printf 'screen-ready\\n'; sleep 30"}),
    );
    assert!(!out.is_error, "{}", out.text);
    let out = bg(
        &tool,
        &ws,
        json!({"action":"wait", "name":"terminal", "until":"screen-ready", "timeout":5}),
    );
    assert!(!out.is_error, "{}", out.text);
    let events = tool.drain_notifications();
    assert!(
        events
            .iter()
            .any(|event| event.kind == "background_progress"
                && event.name == "terminal"
                && event.text.contains("screen-ready"))
    );
    assert!(!bg(&tool, &ws, json!({"action":"kill", "name":"terminal"})).is_error);
}

#[test]
fn pty_start_is_refused_when_no_pty_is_configured() {
    let (_dir, ws) = open_workspace();
    let tool = Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: false,
    };
    let out = bg(
        &tool,
        &ws,
        json!({"action": "start", "name": "p", "command": "true", "pty": true}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("--no-pty"), "{}", out.text);
    assert!(tool.table().procs.is_empty());
}

#[test]
fn pty_capture_survives_gaps_between_output() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    let started = bg(
        &tool,
        &ws,
        json!({"action": "start", "name": "delayed-pty", "pty": true,
            "command": "sleep 0.05; printf 'ready\\n'; sleep 0.05; printf 'done\\n'"}),
    );
    assert!(!started.is_error, "{}", started.text);
    let finished = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "delayed-pty", "timeout": 5}),
    );
    assert!(!finished.is_error, "{}", finished.text);
    assert!(finished.text.contains("exit code 0"), "{}", finished.text);
    let captured = bg(
        &tool,
        &ws,
        json!({"action": "output", "name": "delayed-pty"}),
    );
    assert!(!captured.is_error, "{}", captured.text);
    assert!(
        captured.text.contains("ready") && captured.text.contains("done"),
        "{}",
        captured.text
    );
    assert!(
        !captured.text.contains("output capture error"),
        "{}",
        captured.text
    );
}

fn start(tool: &Background, ws: &Workspace, name: &str, command: &str) -> ToolOutput {
    bg(
        tool,
        ws,
        json!({"action": "start", "name": name, "command": command}),
    )
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait for `pid` to disappear (it is reparented to launchd once killed, which reaps it).
fn gone_soon(pid: i32) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The number after `marker` on the matched line (`> ...`) of a `wait` result.
fn pid_from(text: &str, marker: &str) -> i32 {
    let line = text
        .lines()
        .find(|l| l.starts_with("> "))
        .unwrap_or_else(|| panic!("no match line in {text:?}"));
    let rest = &line[line
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker:?} in {line:?}"))
        + marker.len()..];
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn start_output_wait_and_kill() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    let started = start(
        &tool,
        &ws,
        "ticker",
        "echo tick 1; sleep 0.2; echo tick 2; sleep 30",
    );
    assert!(!started.is_error, "{}", started.text);

    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "ticker", "until": "tick \\d+$", "timeout": 10}),
    );
    assert!(
        !wait.is_error && wait.text.starts_with("ready:"),
        "{}",
        wait.text
    );

    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "ticker", "until": "tick 2", "timeout": 10}),
    );
    assert!(wait.text.contains("> tick 2"), "{}", wait.text);

    let first = bg(&tool, &ws, json!({"action": "output", "name": "ticker"}));
    assert!(
        first.text.contains("tick 1\ntick 2\n") && first.text.contains("running"),
        "{}",
        first.text
    );
    let second = bg(&tool, &ws, json!({"action": "output", "name": "ticker"}));
    assert!(second.text.contains("(no new output)"), "{}", second.text);

    let list = bg(&tool, &ws, json!({"action": "list"}));
    assert!(
        list.text.contains("ticker") && list.text.contains("1 running"),
        "{}",
        list.text
    );

    let killed = bg(&tool, &ws, json!({"action": "kill", "name": "ticker"}));
    assert!(
        !killed.is_error && killed.text.contains("killed ticker"),
        "{}",
        killed.text
    );
    let gone = bg(&tool, &ws, json!({"action": "output", "name": "ticker"}));
    assert!(
        gone.is_error && gone.text.contains("no background process"),
        "{}",
        gone.text
    );
}

#[test]
fn tail_returns_last_lines_of_new_output_only() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "t", "printf 'a\\nb\\nc\\nd\\n'; sleep 30");
    bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "t", "until": "^d$", "timeout": 10}),
    );
    let out = bg(
        &tool,
        &ws,
        json!({"action": "output", "name": "t", "tail": 2}),
    );
    assert!(
        out.text.ends_with("c\nd\n") && !out.text.contains("b\n"),
        "{}",
        out.text
    );
    let again = bg(&tool, &ws, json!({"action": "output", "name": "t"}));
    assert!(again.text.contains("(no new output)"), "{}", again.text);
}

#[test]
fn wait_without_pattern_returns_at_exit_with_status() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "quick", "echo hi; exit 3");
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "quick", "timeout": 10}),
    );
    // The exit result carries the output, so no separate `output` round trip is needed.
    assert!(
        !wait.is_error && wait.text.contains("exit code 3") && wait.text.contains("hi\n"),
        "{}",
        wait.text
    );
    assert_eq!(
        wait.execution.observation.as_ref().unwrap().kind,
        ObservationKind::Wait
    );
    assert!(wait.execution.observation.as_ref().unwrap().satisfied);
    assert_eq!(wait.execution.process.as_ref().unwrap().exit_code, Some(3));
    assert_eq!(wait.data.as_ref().unwrap()["process"]["exit_code"], 3);
    // `output` shares the model-visible cursor: nothing is repeated, but the status still is.
    let out = bg(&tool, &ws, json!({"action": "output", "name": "quick"}));
    assert!(
        out.text.contains("(no new output)") && out.text.contains("exited (exit code 3)"),
        "{}",
        out.text
    );
}

#[test]
fn wait_after_output_returns_only_unread_output() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "two", "echo first; sleep 0.3; echo second");
    bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "two", "until": "^first$", "timeout": 10}),
    );
    bg(&tool, &ws, json!({"action": "output", "name": "two"}));
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "two", "timeout": 10}),
    );
    assert!(
        wait.text.starts_with("ready: two exited (exit code 0)")
            && wait.text.contains("second\n")
            && !wait.text.contains("first"),
        "{}",
        wait.text
    );
}

#[test]
fn wait_reports_timeout_and_early_exit_as_not_ready() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "idle", "sleep 30");
    let timed = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "idle", "until": "never", "timeout": 1}),
    );
    assert!(
        !timed.is_error && timed.text.starts_with("not ready: timed out after 1s"),
        "{}",
        timed.text
    );

    start(&tool, &ws, "dies", "echo nothing useful");
    let early = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "dies", "until": "listening", "timeout": 10}),
    );
    assert!(
        early.is_error && early.text.contains("before /listening/"),
        "{}",
        early.text
    );
}

#[test]
fn bad_arguments_are_errors() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    for args in [
        json!({"action": "dance"}),
        json!({"action": "start"}),
        json!({"action": "start", "command": "  "}),
        json!({"action": "start", "command": "true", "name": "has space"}),
        json!({"action": "wait", "name": "nope"}),
        json!({"action": "kill"}),
        json!({"action": "kill", "name": "x", "all": true}),
        json!({"action": "output", "name": "nope", "tail": 0}),
    ] {
        let out = bg(&tool, &ws, args.clone());
        assert!(out.is_error, "{args} -> {}", out.text);
    }
    start(&tool, &ws, "p", "sleep 30");
    let bad = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "p", "until": "(", "timeout": 1}),
    );
    assert!(
        bad.is_error && bad.text.contains("valid regex"),
        "{}",
        bad.text
    );
}

#[test]
fn output_buffer_is_bounded_and_reports_discarded_bytes() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(
        &tool,
        &ws,
        "flood",
        "head -c 1000000 /dev/zero | tr '\\0' x; echo; echo END; sleep 30",
    );
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "flood", "until": "^END$", "timeout": 20}),
    );
    assert!(wait.text.starts_with("ready:"), "{}", wait.text);
    {
        let table = tool.table();
        let ring = lock(&table.procs[0].ring);
        assert!(ring.total > 1_000_000);
        assert_eq!(ring.text().len(), MAX_BUFFER);
    }
    let out = bg(&tool, &ws, json!({"action": "output", "name": "flood"}));
    assert!(
        out.text.len() < MAX_OUTPUT + 512,
        "{} bytes",
        out.text.len()
    );
    assert!(
        out.text.contains("bytes of earlier output were discarded") && out.text.contains("END"),
        "{}",
        &out.text[..200]
    );
}

#[test]
fn ring_keeps_newest_bytes_and_tracks_what_was_lost() {
    let mut ring = Ring::new(8);
    ring.push(b"0123");
    ring.push(b"456789");
    assert_eq!(ring.text(), "23456789");
    assert_eq!(ring.since(0), (b"23456789".to_vec(), 2));
    assert_eq!(ring.since(5), (b"56789".to_vec(), 0));
    ring.push(b"abcdefghijkl");
    assert_eq!(ring.text(), "efghijkl");
    assert_eq!(ring.total, 22);
    assert_eq!(ring.since(ring.total), (Vec::new(), 0));
}

#[test]
fn ring_oldest_since_reads_forward_and_reports_only_discarded_bytes() {
    let mut ring = Ring::new(8);
    ring.push(b"0123456789");
    assert_eq!(ring.oldest_since(0, 3), (b"234".to_vec(), 2));
    assert_eq!(ring.oldest_since(4, 3), (b"456".to_vec(), 0));
    assert_eq!(ring.oldest_since(7, 100), (b"789".to_vec(), 0));
    assert_eq!(ring.oldest_since(ring.total, 4), (Vec::new(), 0));
}

#[test]
fn terminal_ring_renders_the_current_screen_not_the_byte_stream() {
    let mut ring = Ring::terminal(64, 3, 20);
    ring.push(b"\x1b[31mhi\x1b[0m\r\nthere");
    assert_eq!(ring.screen_text().as_deref(), Some("hi\nthere"));
    ring.push(b"\x1b[2J\x1b[Hnew");
    assert_eq!(ring.screen_text().as_deref(), Some("new"));
    assert_eq!(ring.total, 28, "raw bytes still count every write");
    assert!(Ring::new(8).screen_text().is_none());
}

#[test]
fn terminal_match_text_drops_escapes_and_crlf() {
    assert_eq!(
        plain_terminal("\x1b[31mred\x1b[0m\r\nok\x1b]0;t\x07!\rover"),
        "red\nok!over"
    );
    let pattern = compile("ready$").unwrap();
    let ring = Mutex::new(Ring::terminal(64, 3, 20));
    lock(&ring).push(b"\x1b[1mready\x1b[0m\r\n");
    let mut scanned = None;
    assert_eq!(
        find_match(&ring, true, &pattern, &mut scanned).as_deref(),
        Some("ready")
    );
}

#[test]
fn kill_takes_down_the_whole_process_group() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "tree", "sleep 300 & echo child $!; wait");
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "tree", "until": "child \\d+", "timeout": 10}),
    );
    let child = pid_from(&wait.text, "child ");
    assert!(alive(child));
    bg(&tool, &ws, json!({"action": "kill", "name": "tree"}));
    assert!(gone_soon(child), "grandchild {child} survived kill");
}

#[test]
fn dropping_the_tool_kills_every_process_and_its_children() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    let mut pids = Vec::new();
    for name in ["one", "two"] {
        start(&tool, &ws, name, "sleep 300 & echo child $!; wait");
        let wait = bg(
            &tool,
            &ws,
            json!({"action": "wait", "name": name, "until": "child \\d+", "timeout": 10}),
        );
        pids.push(pid_from(&wait.text, "child "));
        pids.push(tool.table().procs.last().unwrap().pid as i32);
    }
    assert!(pids.iter().all(|&pid| alive(pid)));
    drop(tool);
    for pid in pids {
        assert!(gone_soon(pid), "process {pid} outlived the tool");
    }
}

#[test]
fn a_finished_leader_takes_its_stragglers_with_it() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "leaky", "sleep 300 & echo child $!");
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "leaky", "until": "child \\d+", "timeout": 10}),
    );
    let child = pid_from(&wait.text, "child ");
    bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "leaky", "timeout": 10}),
    );
    assert!(
        gone_soon(child),
        "straggler {child} outlived its finished leader"
    );
}

#[test]
fn process_cap_and_unique_names_are_enforced() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    for i in 0..MAX_PROCESSES {
        let out = start(&tool, &ws, &format!("p{i}"), "sleep 30");
        assert!(!out.is_error, "{}", out.text);
    }
    let over = start(&tool, &ws, "extra", "sleep 30");
    assert!(
        over.is_error && over.text.contains("already running"),
        "{}",
        over.text
    );
    let dup = start(&tool, &ws, "p0", "sleep 30");
    assert!(dup.is_error, "{}", dup.text);

    // Killing one frees a slot; unnamed starts get generated unique names.
    bg(&tool, &ws, json!({"action": "kill", "name": "p0"}));
    let auto = bg(
        &tool,
        &ws,
        json!({"action": "start", "command": "sleep 30"}),
    );
    assert!(
        !auto.is_error && auto.text.starts_with("started bg1 "),
        "{}",
        auto.text
    );
    let killed = bg(&tool, &ws, json!({"action": "kill", "all": true}));
    assert!(killed.text.contains("killed 8 running"), "{}", killed.text);
    assert!(
        bg(&tool, &ws, json!({"action": "list"}))
            .text
            .contains("no background processes")
    );
}

#[test]
fn a_finished_name_can_be_reused_and_old_entries_are_pruned() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    for round in 1..=2 {
        start(&tool, &ws, "job", &format!("echo round {round}"));
        let wait = bg(
            &tool,
            &ws,
            json!({"action": "wait", "name": "job", "timeout": 10}),
        );
        assert!(
            wait.text.contains(&format!("round {round}"))
                && !wait.text.contains(&format!("round {}", 3 - round)),
            "{}",
            wait.text
        );
    }
    for i in 0..20 {
        start(&tool, &ws, &format!("short{i}"), "true");
        bg(
            &tool,
            &ws,
            json!({"action": "wait", "name": format!("short{i}"), "timeout": 10}),
        );
    }
    assert!(tool.table().procs.len() <= 8);
}

#[test]
fn cwd_must_stay_inside_the_workspace() {
    let (dir, ws) = open_workspace();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let tool = tool();
    let ok = bg(
        &tool,
        &ws,
        json!({"action": "start", "name": "pwd", "command": "pwd; sleep 30", "cwd": "sub"}),
    );
    assert!(!ok.is_error, "{}", ok.text);
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "pwd", "until": "/sub$", "timeout": 10}),
    );
    assert!(wait.text.starts_with("ready:"), "{}", wait.text);
    for cwd in ["../elsewhere", "/etc", "missing"] {
        let out = bg(
            &tool,
            &ws,
            json!({"action": "start", "command": "true", "cwd": cwd}),
        );
        assert!(out.is_error, "{cwd}: {}", out.text);
    }
}

#[test]
fn approval_is_needed_to_start_and_kill_only() {
    let tool = tool();
    for (action, approval) in [
        ("start", true),
        ("kill", true),
        ("list", false),
        ("output", false),
        ("wait", false),
        ("bogus", true),
    ] {
        assert_eq!(
            tool.needs_approval(&json!({"action": action})),
            approval,
            "{action}"
        );
    }
    assert!(tool.needs_approval(&json!({})));
}

#[test]
fn start_refuses_when_no_sandbox_is_available() {
    let (_dir, ws) = workspace();
    let ws = ws.with_sandbox(Sandbox::Unavailable("test says no".to_string()));
    let tool = tool();
    let out = start(&tool, &ws, "never", "touch ran.txt");
    assert!(
        out.is_error && out.text.contains("test says no"),
        "{}",
        out.text
    );
    assert!(tool.table().procs.is_empty());
}

#[cfg(target_os = "macos")]
fn require_sandbox() -> bool {
    match Sandbox::detect().available() {
        Ok(()) => true,
        Err(reason) => {
            eprintln!("skipping: sandbox-exec unavailable: {reason}");
            false
        }
    }
}

/// A process under `ws` must write inside the workspace but not outside it.
#[cfg(target_os = "macos")]
fn assert_confined(dir: &Path, ws: &Workspace) {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwn");
    let tool = tool();
    let script = format!(
        "echo inside > made.txt; echo pwn > '{}'; echo finished",
        target.display()
    );
    start(&tool, ws, "jail", &script);
    let wait = bg(
        &tool,
        ws,
        json!({"action": "wait", "name": "jail", "until": "^finished$", "timeout": 20}),
    );
    assert!(wait.text.starts_with("ready:"), "{}", wait.text);
    assert_eq!(
        fs::read_to_string(dir.join("made.txt")).unwrap(),
        "inside\n"
    );
    assert!(
        !target.exists(),
        "background process wrote outside the workspace"
    );
    let out = bg(&tool, ws, json!({"action": "output", "name": "jail"}));
    assert!(
        out.text.to_lowercase().contains("not permitted") || out.text.contains("denied"),
        "{}",
        out.text
    );
}

#[cfg(target_os = "macos")]
#[test]
fn strict_sandbox_confines_background_writes() {
    if !require_sandbox() {
        return;
    }
    let (dir, ws) = workspace();
    assert_confined(dir.path(), &ws);
}

#[cfg(target_os = "macos")]
#[test]
fn dev_sandbox_confines_background_writes() {
    if !require_sandbox() {
        return;
    }
    let (dir, ws) = workspace();
    let policy = Policy::Dev {
        deny_read: Vec::new(),
        extra_write: Vec::new(),
        allow_network: false,
    };
    let sandbox = Sandbox::probe_policy(dir.path(), &policy);
    if let Err(reason) = sandbox.available() {
        eprintln!("skipping: dev sandbox unavailable: {reason}");
        return;
    }
    assert_confined(dir.path(), &ws.with_policy(policy).with_sandbox(sandbox));
}

#[test]
fn heartbeat_emits_start_live_deltas_then_completion_once() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(
        &tool,
        &ws,
        "live",
        "printf 'first\\n'; sleep 0.3; printf 'last\\n'",
    );
    let mut events = tool.drain_notifications();
    assert_eq!(events.first().unwrap().kind, "start");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = super::wake::generation();
        let next = tool.drain_notifications();
        let complete = next.iter().any(|event| event.kind == "complete");
        events.extend(next);
        if complete {
            break;
        }
        assert!(Instant::now() < deadline, "never received completion");
        super::wake::wait_past(seen, Some(deadline));
    }
    let output: String = events
        .iter()
        .filter(|event| event.kind == "background_output")
        .map(|event| event.text.as_str())
        .collect();
    assert_eq!(output, "first\nlast\n");
    assert_eq!(events.last().unwrap().kind, "complete");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "complete")
            .count(),
        1
    );
    assert!(
        tool.drain_notifications().is_empty(),
        "heartbeat repeated old output/completion"
    );
    // Heartbeat consumption must not advance the explicit model-facing output cursor.
    let out = bg(&tool, &ws, json!({"action": "output", "name": "live"}));
    assert!(out.text.contains("first\nlast\n"), "{}", out.text);
}

#[test]
fn background_complete_artifact_preserves_discarded_middle() {
    let (_dir, ws) = open_workspace();
    let artifacts = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
    let ws = ws.with_artifacts(store);
    let tool = tool();
    start(
        &tool,
        &ws,
        "large",
        "printf 'START\\n'; head -c 1048576 /dev/zero; printf '\\nMIDDLE\\n'; head -c 1048576 /dev/zero; printf '\\nEND\\n'",
    );
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "large", "timeout": 10}),
    );
    assert!(!wait.is_error, "{}", wait.text);
    let out = bg(&tool, &ws, json!({"action": "output", "name": "large"}));
    assert!(!out.text.contains("MIDDLE") && out.text.len() < MAX_OUTPUT + 400);
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = super::wake::generation();
        events.extend(tool.drain_notifications());
        if events.iter().any(|event| event.kind == "complete") {
            break;
        }
        assert!(Instant::now() < deadline, "artifact never finished");
        super::wake::wait_past(seen, Some(deadline));
    }
    let complete = events
        .iter()
        .find(|event| event.kind == "complete")
        .unwrap();
    let uri = complete.artifact.as_ref().unwrap();
    let bytes = fs::read(
        artifacts
            .path()
            .join(uri.strip_prefix("artifact://").unwrap()),
    )
    .unwrap();
    assert_eq!(bytes.len(), 2 * 1048576 + b"START\n\nMIDDLE\n\nEND\n".len());
    assert_eq!(&bytes[1048576 + 6..1048576 + 14], b"\nMIDDLE\n");
    assert!(bytes.starts_with(b"START\n") && bytes.ends_with(b"\nEND\n"));
    let output = events
        .iter()
        .find(|event| event.kind == "background_output")
        .unwrap();
    assert!(
        output.text.len() < 16 * 1024 + 200,
        "unbounded heartbeat output"
    );
    assert!(tool.drain_notifications().is_empty());
}

#[test]
fn kill_drains_final_output_before_one_completion_notification() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "killed", "echo ready; sleep 30");
    bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "killed", "until": "ready", "timeout": 5}),
    );
    bg(&tool, &ws, json!({"action": "kill", "name": "killed"}));
    let events = tool.drain_notifications();
    assert_eq!(events.first().unwrap().kind, "start");
    assert!(
        events
            .iter()
            .any(|event| event.kind == "background_output" && event.text.contains("ready"))
    );
    assert_eq!(events.last().unwrap().kind, "complete");
    assert!(tool.drain_notifications().is_empty());
}

#[test]
fn notify_wakes_a_blocked_generation_waiter() {
    let seen = super::wake::generation();
    let waiter = std::thread::spawn(move || super::wake::wait_past(seen, None));
    super::wake::notify();
    waiter.join().unwrap();
}

#[test]
fn wait_past_returns_at_deadline() {
    let start = Instant::now();
    let seen = super::wake::generation();
    super::wake::wait_past(seen, Some(start + Duration::from_millis(20)));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn notify_cancel_interrupts_a_blocked_fd_wait() {
    use std::os::fd::AsRawFd;
    let (read, _write) = std::io::pipe().unwrap();
    let waker = super::wake::Waker::new().unwrap();
    let canceller = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(50));
        super::wake::notify_cancel();
    });
    let start = Instant::now();
    waker.wait(read.as_raw_fd(), libc::POLLIN, None);
    assert!(start.elapsed() < Duration::from_secs(5));
    canceller.join().unwrap();
}

#[test]
fn detached_services_are_not_awaited_and_completion_is_delivered_once() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    assert!(!start(&tool, &ws, "svc", "sleep 30").is_error);
    assert!(!start(&tool, &ws, "job", "echo done").is_error);
    assert!(
        tool.awaited_work().is_empty(),
        "explicit bg start must not be awaited"
    );
    let mut completes = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while completes == 0 && std::time::Instant::now() < deadline {
        completes += tool
            .drain_notifications()
            .iter()
            .filter(|event| event.kind == "complete" && event.name == "job")
            .count();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(completes, 1, "the completion must be reported");
    assert!(
        tool.drain_notifications()
            .iter()
            .all(|event| event.kind != "complete" || event.name != "job"),
        "the completion must not be delivered twice"
    );
    let _ = bg(&tool, &ws, json!({"action": "kill", "name": "svc"}));
}

#[cfg(unix)]
#[test]
fn bash_adopted_job_is_awaited_until_it_is_killed() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    let cancel = AtomicBool::new(false);
    let on_adopt = |job: crate::tools::exec::shell::ForegroundJob| tool.table().adopt(job);
    let out = crate::tools::exec::shell::bash_tool_streaming_with_adopter(
        &ws,
        &json!({"command": "sleep 30", "background": true}),
        0,
        0,
        &|_: &[u8]| {},
        &cancel,
        &on_adopt,
    )
    .unwrap();
    assert!(out.text.contains("background job"), "{}", out.text);
    let awaited = tool.awaited_work();
    assert_eq!(awaited.len(), 1, "{awaited:?}");
    let name = awaited[0].split(' ').next().unwrap_or_default().to_owned();
    let _ = bg(&tool, &ws, json!({"action": "kill", "name": name}));
    assert!(tool.awaited_work().is_empty());
}

/// Ending a `bg wait` never leaves unknown effects: the job keeps running. A new user message
/// (steering) ends it as an ordinary result so the agent continues with the message; a hard
/// interrupt still aborts it, but without the "uncertain effects" stop that refuses further calls.
#[cfg(unix)]
#[test]
fn steering_ends_a_wait_cleanly_and_a_hard_interrupt_still_aborts_it() {
    use crate::tools::exec::signals::{CallSignals, scope};
    use crate::tools::surface::outcome::Effects;
    let (_dir, ws) = open_workspace();
    let tool = tool();
    start(&tool, &ws, "svc", "sleep 30");
    let wait = json!({"action": "wait", "name": "svc", "timeout": 20});
    assert!(tool.interruptible(&wait), "bg wait must be interruptible");

    let steering = CallSignals::new();
    steering.request_steering();
    steering.interrupt_waits();
    let steered = scope(&steering, || {
        crate::tools::exec::execution::run(&AtomicBool::new(false), || bg(&tool, &ws, wait.clone()))
    });
    assert!(!steered.is_error, "{}", steered.text);
    assert_ne!(
        steered.execution.effects,
        Effects::Unknown,
        "{}",
        steered.text
    );
    assert!(steered.text.contains("still running"), "{}", steered.text);
    let listed = bg(&tool, &ws, json!({"action": "list"}));
    assert!(listed.text.contains("svc"), "{}", listed.text);

    let hard = CallSignals::new();
    hard.interrupt_waits();
    let aborted = scope(&hard, || {
        crate::tools::exec::execution::run(&AtomicBool::new(false), || bg(&tool, &ws, wait.clone()))
    });
    assert!(aborted.is_error, "{}", aborted.text);
    assert!(
        aborted.text.contains("cancelled waiting") && !aborted.text.contains("Effects unknown"),
        "{}",
        aborted.text
    );
    assert_eq!(
        (aborted.execution.status, aborted.execution.effects),
        (Some(crate::tools::ToolStatus::Aborted), Effects::None)
    );
    let _ = bg(&tool, &ws, json!({"action": "kill", "name": "svc"}));
}

#[cfg(unix)]
#[test]
fn escaped_writer_is_drained_bounded_and_kill_yields_one_incomplete_artifact() {
    let setsid_available = std::process::Command::new("setsid")
        .arg("true")
        .status()
        .is_ok_and(|status| status.success());
    if !setsid_available {
        eprintln!("setsid is unavailable; skipping the escaped-writer fixture");
        return;
    }
    let (_dir, ws) = open_workspace();
    let artifacts = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
    let ws = ws.with_artifacts(store);
    let tool = tool();
    // The inner shell starts its own session, so it survives the group kill and keeps the pipe open.
    start(
        &tool,
        &ws,
        "escaped",
        "setsid sh -c 'echo \"escaped $$\"; printf \"kept\\n\"; sleep 60' & sleep 0.3; echo leader",
    );
    let wait = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "escaped", "until": "escaped \\d+", "timeout": 10}),
    );
    let child = pid_from(&wait.text, "escaped ");
    let kept = bg(
        &tool,
        &ws,
        json!({"action": "wait", "name": "escaped", "until": "kept", "timeout": 10}),
    );
    assert!(!kept.is_error, "{}", kept.text);

    let began = Instant::now();
    let killed = bg(&tool, &ws, json!({"action": "kill", "name": "escaped"}));
    assert!(!killed.is_error, "{}", killed.text);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "kill waited {:?} for the reader",
        began.elapsed()
    );
    // Dial's reader and spool are released, while the escaped writer is outside the group.
    assert!(
        alive(child),
        "the escaped writer should survive the group kill"
    );

    let mut completions = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = super::wake::generation();
        completions.extend(
            tool.drain_notifications()
                .into_iter()
                .filter(|event| event.kind == "complete"),
        );
        if !completions.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the reader never completed");
        super::wake::wait_past(seen, Some(deadline));
    }
    assert!(
        tool.drain_notifications()
            .iter()
            .all(|event| event.kind != "complete"),
        "completion must be delivered once"
    );
    assert_eq!(completions.len(), 1);
    let complete = &completions[0];
    assert!(complete.text.contains("incomplete"), "{}", complete.text);
    let uri = complete.artifact.as_ref().expect("a partial artifact");
    let bytes = fs::read(
        artifacts
            .path()
            .join(uri.strip_prefix("artifact://").unwrap()),
    )
    .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("escaped ") && text.contains("kept"), "{text}");

    // Reap the fixture explicitly; it is not in any group Dial can signal.
    unsafe {
        libc::kill(child, libc::SIGKILL);
    }
    assert!(gone_soon(child), "escaped writer {child} survived SIGKILL");
}

#[test]
fn jobs_view_shows_pipe_output_without_consuming_it() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    assert!(!start(&tool, &ws, "pipe-view", "printf 'first\\nlast\\n'").is_error);
    let snap = tool.table().snapshot("pipe-view").unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = wake::generation();
        if lock(&snap.ring).closed {
            break;
        }
        assert!(Instant::now() < end, "capture did not close");
        wake::wait_past(seen, Some(end));
    }
    let view = tool.jobs_view(Some("pipe-view"));
    assert_eq!(view.screen, ["first", "last"]);
    assert_eq!(view.cursor, None);
    let output = bg(&tool, &ws, json!({"action":"output", "name":"pipe-view"}));
    assert!(output.text.contains("first\nlast\n"), "{}", output.text);
}

#[test]
fn wait_reports_capture_failure_as_an_error() {
    let (_dir, ws) = open_workspace();
    let tool = tool();
    assert!(!start(&tool, &ws, "capture-failed", "true").is_error);
    let finished = bg(
        &tool,
        &ws,
        json!({"action":"wait", "name":"capture-failed", "timeout":5}),
    );
    assert!(!finished.is_error, "{}", finished.text);
    let snap = tool.table().snapshot("capture-failed").unwrap();
    lock(&snap.ring).error = Some("test capture failure".into());
    let output = bg(
        &tool,
        &ws,
        json!({"action":"wait", "name":"capture-failed", "timeout":5}),
    );
    assert!(output.is_error, "{}", output.text);
    assert!(
        output.text.contains("test capture failure"),
        "{}",
        output.text
    );
}

#[test]
fn capped_artifact_is_not_labeled_complete() {
    let (_dir, ws) = open_workspace();
    let storage = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(storage.path())
        .unwrap()
        .with_limits(&crate::storage::artifact::ArtifactConfig {
            max_bytes: 8,
            head_bytes: 4,
        });
    let ws = ws.with_artifacts(store);
    let tool = tool();
    start(&tool, &ws, "cap", "printf 0123456789abcdef");
    let out = bg(
        &tool,
        &ws,
        json!({"action":"wait","name":"cap","timeout":5}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("[partial output: artifact://"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("[complete output:"));
    let integrity = out.execution.output.unwrap();
    assert_eq!(integrity.capture, Completeness::Complete);
    assert_eq!(integrity.retention, Completeness::Partial);
    assert_eq!(integrity.original_bytes, Some(16));
    assert_eq!(integrity.retained_bytes, Some(8));
    assert_eq!(integrity.omitted_bytes, Some(8));
}

fn minimizer_snapshot() -> Snapshot {
    use std::os::unix::process::ExitStatusExt;
    let mut ring = Ring::new(MAX_BUFFER);
    ring.push("   Compiling dependency v1.0.0\n".repeat(1500).as_bytes());
    ring.push(b"warning: keep this diagnostic\n    Finished dev profile\n");
    ring.closed = true;
    ring.reached_eof = true;
    ring.artifact = Some("artifact://test-output".into());
    Snapshot {
        name: "build".into(),
        command: "cargo build".into(),
        pid: 1,
        ring: Arc::new(Mutex::new(ring)),
        state: State::Exited {
            at: Instant::now(),
            detail: "exit code 0".into(),
            status: Some(std::process::ExitStatus::from_raw(0)),
        },
        network: false,
        terminal: false,
    }
}

#[test]
fn settled_background_minimizer_preserves_diagnostics_facts_and_cursor() {
    let (_dir, ws) = open_workspace();
    let ws = ws.with_shell_config(&crate::config::types::ShellConfig {
        minimize_output: true,
        ..Default::default()
    });
    let mut snap = minimizer_snapshot();
    snap.command = "cargo fmt --check && cargo check && cargo test".into();
    let original = lock(&snap.ring).total;
    assert!(original > MAX_OUTPUT as u64);
    let (body, lost, integrity) = take_presented(&ws, &snap, None);
    assert!(body.contains("[output filtered (cargo)"), "{body}");
    assert!(body.contains("warning: keep this diagnostic"));
    assert!(!body.contains("Compiling"));
    assert_eq!(lost, 0);
    assert_eq!(integrity.capture, Completeness::Complete);
    assert_eq!(integrity.retention, Completeness::Complete);
    assert_eq!(integrity.presentation, Completeness::Partial);
    assert_eq!(integrity.original_bytes, Some(original));
    assert_eq!(integrity.retained_bytes, Some(original));
    assert_eq!(integrity.omitted_bytes, Some(0));
    assert_eq!(process_outcome(&snap).exit_code, Some(0));
    assert!(process_outcome(&snap).output_complete);
    assert!(render_body(&snap, body, lost, None).contains("[complete output: artifact://"));
    let (again, lost, _) = take_presented(&ws, &snap, None);
    assert!(again.is_empty());
    assert_eq!(lost, 0);
}

#[test]
fn background_minimizer_refuses_partial_delta_pty_unknown_and_disabled_results() {
    let (_dir, ws) = open_workspace();
    let ws = ws.with_shell_config(&crate::config::types::ShellConfig {
        minimize_output: true,
        ..Default::default()
    });
    for case in 0..11 {
        let mut snap = minimizer_snapshot();
        let mut tail = None;
        let mut ws = ws.clone();
        match case {
            0 => lock(&snap.ring).artifact = None,
            1 => lock(&snap.ring).reached_eof = false,
            2 => lock(&snap.ring).closed = false,
            3 => lock(&snap.ring).error = Some("capture failed".into()),
            4 => lock(&snap.ring).elided_bytes = 1,
            5 => lock(&snap.ring).cursor = 1,
            6 => snap.terminal = true,
            7 => snap.state = State::Running,
            8 => {
                snap.state = State::Exited {
                    at: Instant::now(),
                    detail: "exit status unavailable".into(),
                    status: None,
                }
            }
            9 => tail = Some(2),
            10 => {
                ws = ws.with_shell_config(&crate::config::types::ShellConfig {
                    minimize_output: false,
                    ..Default::default()
                })
            }
            _ => unreachable!(),
        }
        let (body, _, _) = take_presented(&ws, &snap, tail);
        assert!(!body.contains("[output filtered"), "case {case}: {body}");
        assert!(body.contains("Compiling"), "case {case}: {body}");
    }
    let mut snap = minimizer_snapshot();
    snap.command = "cargo build && cat important.txt".into();
    let (body, _, _) = take_presented(&ws, &snap, None);
    assert!(body.contains("Compiling"));
    assert!(!body.contains("[output filtered"));
}

#[test]
fn background_wait_and_output_minimize_complete_saved_builds() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, ws) = open_workspace();
    let storage = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(storage.path()).unwrap();
    let ws = ws
        .with_artifacts(store)
        .with_shell_config(&crate::config::types::ShellConfig {
            minimize_output: true,
            ..Default::default()
        });
    let script = dir.path().join("cargo");
    fs::write(&script, "#!/bin/sh\ni=0\nwhile [ $i -lt 1500 ]; do echo '    Checking dependency v1.0.0'; i=$((i+1)); done\necho 'test module::working ... ok'\necho 'test module::working2 ... ok'\necho 'test module::working3 ... ok'\necho 'warning: keep this diagnostic'\nif [ \"$2\" = 101 ]; then echo 'test module::broken ... FAILED'; echo 'test result: FAILED. 3 passed; 1 failed'; fi\nexit \"$2\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let tool = tool();
    for (action, code) in [("wait", 0), ("output", 0), ("wait", 101), ("output", 101)] {
        assert!(
            !start(
                &tool,
                &ws,
                action,
                &format!("{} test {code}", script.display())
            )
            .is_error
        );
        if action == "output" {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = tool.table().snapshot(action).unwrap();
                if matches!(snap.state, State::Exited { .. }) {
                    snap.settle();
                    break;
                }
                assert!(Instant::now() < end, "build did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let out = bg(
            &tool,
            &ws,
            json!({"action": action, "name": action, "timeout":5}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("[output filtered (cargo)"),
            "{}",
            out.text
        );
        assert!(out.text.contains("warning: keep this diagnostic"));
        assert!(!out.text.contains("Checking"));
        assert_eq!(
            out.execution.process.as_ref().unwrap().exit_code,
            Some(code)
        );
        if code != 0 {
            assert!(out.text.contains("module::broken ... FAILED"));
            assert!(out.text.contains("test result: FAILED"));
        }
        let uri = out
            .execution
            .output
            .as_ref()
            .unwrap()
            .artifact
            .as_ref()
            .unwrap();
        let raw = fs::read_to_string(
            storage
                .path()
                .join(uri.strip_prefix("artifact://").unwrap()),
        )
        .unwrap();
        assert_eq!(raw.matches("Checking").count(), 1500);
        let notices = tool.drain_notifications();
        let replay = notices
            .iter()
            .find(|event| event.kind == "background_output")
            .unwrap();
        assert!(
            replay.text.contains("[output filtered (cargo)"),
            "{}",
            replay.text
        );
        assert!(replay.text.contains("warning: keep this diagnostic"));
        assert!(!replay.text.contains("Checking"));
        if code != 0 {
            assert!(replay.text.contains("module::broken ... FAILED"));
        }
        assert!(notices.iter().any(|event| event.kind == "complete"));
        assert!(tool.drain_notifications().is_empty());
        let again = bg(&tool, &ws, json!({"action":"output", "name":action}));
        assert!(again.text.contains("(no new output)"), "{}", again.text);
    }
}

#[test]
fn failed_background_minimizer_preserves_failure_and_capture_without_replay() {
    use std::os::unix::process::ExitStatusExt;
    let (_dir, ws) = open_workspace();
    let ws = ws.with_shell_config(&crate::config::types::ShellConfig {
        minimize_output: true,
        ..Default::default()
    });
    let mut snap = minimizer_snapshot();
    snap.command = "cargo test".into();
    snap.state = State::Exited {
        at: Instant::now(),
        detail: "exit code 101".into(),
        status: Some(std::process::ExitStatus::from_raw(101 << 8)),
    };
    let raw = {
        let mut ring = lock(&snap.ring);
        ring.push("test module::working ... ok\n".repeat(80).as_bytes());
        ring.push(b"test module::broken ... FAILED\nthread 'broken' panicked at src/lib.rs:3\nassertion left: 1 right: 2\ntest result: FAILED. 80 passed; 1 failed\n");
        ring.text()
    };
    let (body, lost, integrity) = take_presented(&ws, &snap, None);
    assert!(body.contains("[output filtered (cargo)"), "{body}");
    assert!(!body.contains("Compiling") && !body.contains("working ... ok"));
    for kept in [
        "broken ... FAILED",
        "panicked at src/lib.rs:3",
        "assertion left: 1 right: 2",
        "test result: FAILED",
    ] {
        assert!(body.contains(kept), "{kept}: {body}");
    }
    assert_eq!(lost, 0);
    assert_eq!(integrity.capture, Completeness::Complete);
    assert_eq!(integrity.retention, Completeness::Complete);
    assert_eq!(integrity.presentation, Completeness::Partial);
    assert_eq!(integrity.omitted_bytes, Some(0));
    assert_eq!(integrity.original_bytes, Some(raw.len() as u64));
    assert_eq!(lock(&snap.ring).text(), raw);
    assert_eq!(process_outcome(&snap).exit_code, Some(101));
    assert!(process_outcome(&snap).output_complete);
    let (again, lost, _) = take_presented(&ws, &snap, None);
    assert!(again.is_empty());
    assert_eq!(lost, 0);
}
