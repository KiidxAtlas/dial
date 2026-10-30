use super::*;
use crate::config::types::Config;
use crate::tools::exec::background::table::Table;
use crate::tools::testkit::workspace;
use std::fs::{self as stdfs, OpenOptions};
use std::io::Write;
use std::sync::atomic::AtomicBool;

fn call(monitor: &Monitor, workspace: &Workspace, args: Value) -> ToolOutput {
    monitor.call(
        &ToolContext {
            workspace,
            config: &Config::default(),
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    )
}

fn append(path: &std::path::Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn start_file(monitor: &Monitor, workspace: &Workspace) {
    let out = call(
        monitor,
        workspace,
        json!({"action":"start", "name":"watch", "path":"server.log"}),
    );
    assert!(!out.is_error, "{}", out.text);
}

#[test]
fn file_only_reads_new_output_and_matches_partial_escaped_lines_once() {
    let (dir, ws) = workspace();
    let path = dir.path().join("server.log");
    stdfs::write(&path, "error from before approval\n").unwrap();
    let monitor = Monitor::new(None);
    start_file(&monitor, &ws);
    assert!(monitor.poll_monitors().is_empty());
    append(&path, b"healthy\n\x1b[3");
    assert!(monitor.poll_monitors().is_empty());
    append(&path, b"1mERR");
    assert!(monitor.poll_monitors().is_empty());
    append(&path, b"OR\x1b[0m: connection lost");
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    assert!(
        events[0].contains("ERROR: connection lost"),
        "{}",
        events[0]
    );
    assert!(events[0].contains("UNTRUSTED"));
    assert!(!events[0].contains("\x1b"));
    assert!(!events[0].contains("before approval"));
    assert!(!monitor.has_monitors());
    append(&path, b"\nerror again\n");
    assert!(monitor.poll_monitors().is_empty());
}

#[test]
fn invalid_sources_and_regexes_never_arm() {
    let (dir, ws) = workspace();
    stdfs::write(dir.path().join("server.log"), "").unwrap();
    let monitor = Monitor::new(None);
    for args in [
        json!({"action":"start", "name":"w"}),
        json!({"action":"start", "name":"w", "path":"server.log", "job":"x"}),
        json!({"action":"start", "name":"w", "path":"missing.log"}),
        json!({"action":"start", "name":"w", "path":"."}),
        json!({"action":"start", "name":"w", "path":"../outside.log"}),
        json!({"action":"start", "name":"w", "job":"missing"}),
        json!({"action":"start", "name":"w", "path":"server.log", "pattern":"["}),
        json!({"action":"start", "name":"w", "path":"server.log", "pattern":"a".repeat(MAX_PATTERN + 1)}),
        json!({"action":"start", "name":"w", "path":"server.log", "pattern":""}),
        json!({"action":"start", "name":"w", "path":"server.log", "pattern":"a{999999999}"}),
        json!({"action":"start", "name":"w", "path":"server.log", "duration_secs":0}),
        json!({"action":"start", "name":"w", "path":"server.log", "duration_secs":86401}),
    ] {
        let out = call(&monitor, &ws, args.clone());
        assert!(out.is_error, "accepted {args}: {}", out.text);
        assert!(!monitor.has_monitors());
    }
}

#[test]
fn stop_expiry_limit_and_fork_are_independent() {
    let (dir, ws) = workspace();
    stdfs::write(dir.path().join("server.log"), "").unwrap();
    let monitor = Monitor::new(None);
    assert!(monitor.needs_approval(&json!({"action":"start"})));
    assert!(!monitor.needs_approval(&json!({"action":"list"})));
    assert!(!monitor.needs_approval(&json!({"action":"stop"})));
    start_file(&monitor, &ws);
    let duplicate = call(
        &monitor,
        &ws,
        json!({"action":"start", "name":"watch", "path":"server.log"}),
    );
    assert!(duplicate.is_error);
    let fork = monitor.fork_session().unwrap();
    assert!(!fork.has_monitors());
    assert!(monitor.has_monitors());
    let out = call(&monitor, &ws, json!({"action":"stop", "name":"watch"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(!monitor.has_monitors());
    start_file(&monitor, &ws);
    lock(&monitor.watches)[0].expires = Instant::now() - Duration::from_secs(1);
    append(&dir.path().join("server.log"), b"error expired\n");
    assert!(monitor.poll_monitors().is_empty());
    assert!(!monitor.has_monitors());
    for index in 0..MAX_WATCHES {
        let out = call(
            &monitor,
            &ws,
            json!({"action":"start", "name":format!("w{index}"), "path":"server.log"}),
        );
        assert!(!out.is_error, "{}", out.text);
    }
    assert!(
        call(
            &monitor,
            &ws,
            json!({"action":"start", "name":"overflow", "path":"server.log"})
        )
        .is_error
    );
    monitor.stop_monitors();
    assert!(!monitor.has_monitors());
}

#[test]
fn rotation_truncation_deletion_and_long_lines_stop_with_diagnostics() {
    for failure in ["rotation", "truncation", "deletion", "long-line"] {
        let (dir, ws) = workspace();
        let path = dir.path().join("server.log");
        stdfs::write(&path, "old output\n").unwrap();
        let monitor = Monitor::new(None);
        start_file(&monitor, &ws);
        match failure {
            "rotation" => {
                stdfs::rename(&path, dir.path().join("previous.log")).unwrap();
                stdfs::write(&path, "error in replacement\n").unwrap();
            }
            "truncation" => stdfs::write(&path, "").unwrap(),
            "deletion" => stdfs::remove_file(&path).unwrap(),
            "long-line" => append(&path, &vec![b'a'; MAX_LINE + 1]),
            _ => unreachable!(),
        }
        let events = monitor.poll_monitors();
        assert_eq!(events.len(), 1, "{failure}");
        assert!(
            events[0].contains("source stopped"),
            "{failure}: {}",
            events[0]
        );
        assert!(!monitor.has_monitors());
    }
}

#[cfg(unix)]
#[test]
fn fifo_and_changed_symlink_never_read_or_block() {
    use std::ffi::CString;
    use std::os::unix::fs::symlink;
    let (dir, ws) = workspace();
    let path = dir.path().join("fifo");
    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: cpath is a valid terminated path for the duration of mkfifo.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let monitor = Monitor::new(None);
    let out = call(
        &monitor,
        &ws,
        json!({"action":"start", "name":"fifo", "path":"fifo"}),
    );
    assert!(out.is_error);
    let outside = tempfile::tempdir().unwrap();
    stdfs::write(outside.path().join("secret.log"), "error private").unwrap();
    stdfs::write(dir.path().join("inside.log"), "").unwrap();
    let link = dir.path().join("server.log");
    symlink(dir.path().join("inside.log"), &link).unwrap();
    start_file(&monitor, &ws);
    stdfs::remove_file(&link).unwrap();
    symlink(outside.path().join("secret.log"), &link).unwrap();
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("source stopped"));
    assert!(!events[0].contains("error private"));
}

fn ring_watch(ring: Arc<Mutex<Ring>>) -> Watch {
    let cursor = lock(&ring).total;
    Watch {
        name: "ring".into(),
        source: Source::Job {
            name: "job".into(),
            ring,
            cursor,
        },
        pattern: Regex::new(DEFAULT_PATTERN).unwrap(),
        expires: Instant::now() + Duration::from_secs(60),
        pending: Vec::new(),
        context: String::new(),
        artifacts: None,
    }
}

#[test]
fn bg_watch_cursor_does_not_consume_output_and_reports_loss_or_close() {
    let ring = Arc::new(Mutex::new(Ring::new(64)));
    lock(&ring).push(b"error old\n");
    let mut watch = ring_watch(ring.clone());
    assert!(matches!(watch.poll().unwrap(), Poll::Pending));
    lock(&ring).push(b"\x1b[31merr");
    assert!(matches!(watch.poll().unwrap(), Poll::Pending));
    lock(&ring).push(b"or\x1b[0m new\n");
    assert!(matches!(watch.poll().unwrap(), Poll::Matched(text) if text.contains("error new")));
    assert_eq!(
        lock(&ring).cursor,
        0,
        "ordinary output cursor must not advance"
    );
    let mut watch = ring_watch(ring.clone());
    lock(&ring).push(&[b'a'; 100]);
    assert!(watch.poll().unwrap_err().contains("overrun"));
    let mut watch = ring_watch(ring.clone());
    lock(&ring).closed = true;
    assert!(matches!(watch.poll().unwrap(), Poll::Closed));
}

#[test]
fn bg_watch_scans_a_burst_larger_than_one_read_without_reporting_overrun() {
    let ring = Arc::new(Mutex::new(Ring::new(MAX_READ * 4)));
    let mut watch = ring_watch(ring.clone());
    let mut burst = b"healthy\n".repeat(MAX_READ / 8 + 2);
    burst.extend_from_slice(b"error later\n");
    lock(&ring).push(&burst);
    lock(&ring).closed = true;
    assert!(matches!(watch.poll().unwrap(), Poll::Pending));
    assert!(matches!(watch.poll().unwrap(), Poll::Matched(text) if text.contains("error later")));
}

#[test]
fn bg_sources_share_session_owner_but_forks_cannot_access_it() {
    let (dir, ws) = workspace();
    let background = Arc::new(Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: true,
    });
    let monitor = Monitor::new(Some(background.clone()));
    // A missing job is rejected through the real shared table, not treated as a file.
    let out = call(
        &monitor,
        &ws,
        json!({"action":"start", "name":"w", "job":"missing"}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("no background process"), "{}", out.text);
    let fork = monitor.fork_session().unwrap();
    let out = fork.call(
        &ToolContext {
            workspace: &ws,
            config: &Config::default(),
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({"action":"start", "name":"w", "job":"missing"}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("forked session"));
    assert_eq!(Arc::strong_count(&background), 2);
    stdfs::write(dir.path().join("server.log"), "").unwrap();
    start_file(&monitor, &ws);
}

#[test]
fn polling_is_bounded_and_list_stop_do_not_read_sources() {
    let (dir, ws) = workspace();
    let path = dir.path().join("server.log");
    stdfs::write(&path, "").unwrap();
    let monitor = Monitor::new(None);
    start_file(&monitor, &ws);
    let mut burst = b"healthy\n".repeat(MAX_READ / 8 + 2);
    burst.extend_from_slice(b"error later\n");
    append(&path, &burst);
    assert!(monitor.poll_monitors().is_empty());
    assert_eq!(monitor.poll_monitors().len(), 1);
    start_file(&monitor, &ws);
    stdfs::remove_file(&path).unwrap();
    let listed = call(&monitor, &ws, json!({"action":"list"}));
    assert!(!listed.is_error);
    assert!(listed.text.contains("watch"));
    assert!(!call(&monitor, &ws, json!({"action":"stop", "name":"watch"})).is_error);
    assert!(monitor.poll_monitors().is_empty());
}

#[test]
fn traceback_diagnostic_includes_surrounding_lines_and_stays_bounded() {
    let (dir, ws) = workspace();
    let path = dir.path().join("server.log");
    stdfs::write(&path, "").unwrap();
    let monitor = Monitor::new(None);
    start_file(&monitor, &ws);
    append(&path, b"request /index\n");
    assert!(monitor.poll_monitors().is_empty());
    append(
        &path,
        b"Traceback (most recent call last):\n  File app.py, line 12\nValueError: bad input\n",
    );
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("request /index"));
    assert!(events[0].contains("ValueError: bad input"));
    assert!(events[0].len() <= 4096);
    assert!(!monitor.may_modify_workspace(&json!({"action":"start"})));
    start_file(&monitor, &ws);
    append(&path, format!("error {}\n", "😀".repeat(1800)).as_bytes());
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    assert!(events[0].len() <= 4096);
}

#[test]
fn normal_bg_eof_silently_disarms_without_waking_for_repairs() {
    let monitor = Monitor::new(None);
    let ring = Arc::new(Mutex::new(Ring::new(64)));
    lock(&monitor.watches).push(ring_watch(ring.clone()));
    lock(&ring).closed = true;
    assert!(monitor.poll_monitors().is_empty());
    assert!(!monitor.has_monitors());
}

#[test]
fn matched_window_retains_reopenable_sanitized_evidence_with_partial_coverage() {
    use crate::runtime::evidence::{CoverageExtent, Representation};
    use crate::storage::artifact::ArtifactStore;
    let (dir, ws) = workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let path = dir.path().join("server.log");
    stdfs::write(&path, "old error\n").unwrap();
    let monitor = Monitor::new(None);
    start_file(&monitor, &ws);
    append(&path, b"healthy\n\x1b[31mERROR: disconnected\x1b[0m\n");
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    let reference = events[0]
        .split("Evidence citation: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let reopened = ArtifactStore::new(state.path()).unwrap();
    let record = reopened.evidence_record(reference).unwrap();
    let bytes = reopened.evidence_bytes(&record, 4096).unwrap();
    assert_eq!(bytes, b"healthy\nERROR: disconnected\n");
    assert_eq!(
        record.evidence.representation,
        Representation::of_bytes("text/plain; charset=utf-8", &bytes)
    );
    assert_eq!(
        record.evidence.context.coverage.unwrap().extent,
        CoverageExtent::Partial
    );
    assert!(record.evidence.context.snapshot.is_none());
    assert!(!events[0].contains('\x1b'));
    assert!(!monitor.has_monitors());
    assert!(monitor.poll_monitors().is_empty());
}

#[test]
fn retention_failure_still_delivers_match_and_disarms_once() {
    use crate::storage::artifact::{ArtifactConfig, ArtifactStore};
    let (dir, ws) = workspace();
    let state = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(state.path())
        .unwrap()
        .with_limits(&ArtifactConfig {
            max_bytes: 1,
            head_bytes: 1,
        });
    let ws = ws.with_artifacts(store);
    let path = dir.path().join("server.log");
    stdfs::write(&path, "").unwrap();
    let monitor = Monitor::new(None);
    start_file(&monitor, &ws);
    append(&path, b"ERROR: disconnected\n");
    let events = monitor.poll_monitors();
    assert_eq!(events.len(), 1);
    assert!(events[0].contains("ERROR: disconnected"));
    assert!(events[0].contains("Evidence retention failed"));
    assert!(!events[0].contains("Evidence citation:"));
    assert!(!monitor.has_monitors());
    append(&path, b"ERROR: second failure\n");
    assert!(monitor.poll_monitors().is_empty());
}

#[test]
fn named_run_watchers_have_independent_cursors_and_stop_never_controls_app() {
    use crate::config::paths::Layout;
    use crate::storage::process_capture::{Capture, Reader};
    let (dir, ws) = workspace();
    let private = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(dir.path(), private.path().join("private")).unwrap();
    let mut capture = Capture::start(&layout, "web").unwrap();
    capture.append(b"ERROR: historical\n").unwrap();
    let first = Monitor::new(None);
    let second = Monitor::new(None);
    let pattern = Regex::new(DEFAULT_PATTERN).unwrap();
    for monitor in [&first, &second] {
        assert!(monitor.needs_approval(&json!({"action":"start", "run":"web"})));
        lock(&monitor.watches).push(Watch {
            name: "web-errors".into(),
            source: Source::Run {
                name: "web".into(),
                reader: Reader::attach(&layout, "web").unwrap(),
            },
            pattern: pattern.clone(),
            expires: Instant::now() + Duration::from_secs(60),
            pending: Vec::new(),
            context: String::new(),
            artifacts: None,
        });
        assert!(monitor.poll_monitors().is_empty());
    }
    capture.append(b"\x1b[31mERROR: fresh\x1b[0m\n").unwrap();
    for monitor in [&first, &second] {
        let events = monitor.poll_monitors();
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("ERROR: fresh"));
        assert!(!events[0].contains("historical"));
        assert!(!events[0].contains('\x1b'));
        assert!(!monitor.has_monitors());
    }
    first.stop_monitors();
    drop(second);
    capture.append(b"still running\n").unwrap();
    assert!(Reader::attach(&layout, "web").is_ok());
    let out = call(
        &first,
        &ws,
        json!({"action":"start","name":"bad","run":"web","path":"x"}),
    );
    assert!(out.is_error && out.text.contains("exactly one"));
}

#[test]
fn named_run_is_additive_for_settled_monitor_history() {
    use crate::tools::integrations::ext::Registry;
    use crate::tools::surface::contracts::{identities, validate_history};
    let registry = Registry::new();
    let current = Monitor::new(None).schema();
    let mut old = current.clone();
    assert!(
        old["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("run")
            .is_some()
    );
    let identity = identities(&[old], &registry)["monitor"].clone();
    let history = vec![json!({"role":"assistant", "dial_tool_contracts":{"call":identity}})];
    validate_history(&history, &[current], &registry).unwrap();
}
