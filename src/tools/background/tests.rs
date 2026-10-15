use super::table::{MAX_BUFFER, MAX_PROCESSES, Ring};
use super::*;
use crate::tools::sandbox::{Policy, Sandbox};
use crate::tools::testkit::workspace;
use std::fs;
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
    assert!(
        !wait.is_error && wait.text.contains("exit code 3"),
        "{}",
        wait.text
    );
    // The last output is available even though the process is already gone.
    let out = bg(&tool, &ws, json!({"action": "output", "name": "quick"}));
    assert!(
        out.text.contains("hi\n") && out.text.contains("exited (exit code 3)"),
        "{}",
        out.text
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
        bg(
            &tool,
            &ws,
            json!({"action": "wait", "name": "job", "timeout": 10}),
        );
        let out = bg(&tool, &ws, json!({"action": "output", "name": "job"}));
        assert!(
            out.text.contains(&format!("round {round}"))
                && !out.text.contains(&format!("round {}", 3 - round)),
            "{}",
            out.text
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
        .filter(|event| event.kind == "output")
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
    let store = crate::tools::artifact::ArtifactStore::new(artifacts.path()).unwrap();
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
    let output = events.iter().find(|event| event.kind == "output").unwrap();
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
            .any(|event| event.kind == "output" && event.text.contains("ready"))
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
    let on_adopt = |job: crate::tools::shell::ForegroundJob| tool.table().adopt(job);
    let out = crate::tools::shell::bash_tool_streaming_with_adopter(
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
