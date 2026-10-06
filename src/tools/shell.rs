//! The `bash` tool: sandboxed process groups, live output and complete artifact capture.

use super::sandbox::{Policy, Sandbox, is_secret_env};
use super::{SCRATCH_DIR, ToolOutput, Workspace, opt_u64, req_str};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_COMMAND: usize = 64 * 1024;
const BASH_HEAD: usize = 6 * 1024;
const BASH_TAIL: usize = 14 * 1024;
/// Diagnostic lines kept from the omitted middle (a compiler's first error is often there).
const BASH_DIAGNOSTICS: usize = 3 * 1024;
const CLT_DIR: &str = "/Library/Developer/CommandLineTools";

/// Kills the whole process group on drop so nothing outlives the tool call.
pub(super) struct ProcessGroup {
    pub(super) child: Child,
    terminated: bool,
}

impl ProcessGroup {
    pub(super) fn new(child: Child) -> Self {
        ProcessGroup {
            child,
            terminated: false,
        }
    }

    pub(super) fn terminate(&mut self) {
        if !self.terminated {
            // The leader is not reaped until after killpg, so its PID cannot be reused.
            unsafe { libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL) };
            self.terminated = true;
        }
    }

    /// True once the leader has exited. It is not reaped, so its PID stays reserved.
    pub(super) fn exited(&self) -> io::Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}

/// Keeps the first and last bytes of a stream, counting what was dropped, plus the diagnostic lines
/// (errors, panics, failed tests) found in the dropped middle.
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    diagnostics: super::diagnostics::Collector,
}

impl Capture {
    fn new() -> Self {
        Capture {
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            diagnostics: super::diagnostics::Collector::new(BASH_DIAGNOSTICS),
        }
    }

    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < BASH_HEAD {
            let take = (BASH_HEAD - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > 2 * BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
    }

    fn drop_front(&mut self, excess: usize) {
        self.diagnostics.feed(&self.tail[..excess]);
        self.tail.drain(..excess);
    }

    fn finish(mut self) -> String {
        if self.tail.len() > BASH_TAIL {
            self.drop_front(self.tail.len() - BASH_TAIL);
        }
        self.diagnostics.finish();
        let dropped = self.total - self.head.len() - self.tail.len();
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        let (kept, lines, overflow) = self.diagnostics.kept();
        if dropped > 0 && lines > 0 {
            let more = if overflow > 0 {
                format!(" ({overflow} more not shown)")
            } else {
                String::new()
            };
            text.push_str(&format!("\n[... {dropped} bytes of output omitted; {lines} diagnostic line(s) from that part follow{more} ...]\n{kept}[... end of omitted-part diagnostics ...]\n"));
        } else if dropped > 0 {
            text.push_str(&format!("\n[... {dropped} bytes of output omitted ...]\n"));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        text
    }
}

/// Read what is available without blocking. Returns true at EOF.
fn drain_pipe(
    reader: &mut impl Read,
    capture: &mut Capture,
    spool: &mut Option<super::artifact::ArtifactSpool>,
    on_output: &(dyn Fn(&[u8]) + Sync),
) -> io::Result<bool> {
    let mut buffer = [0u8; 16 * 1024];
    // Bounded work per pass so a noisy child cannot starve the deadline check.
    for _ in 0..16 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                // Save the complete stream before any display-only clipping.
                if let Some(spool) = spool {
                    spool.write_all(&buffer[..count])?;
                }
                capture.push(&buffer[..count]);
                on_output(&buffer[..count]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

fn wait_readable(fd: i32, millis: i32) {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut poll, 1, millis) };
}

fn bash_path() -> String {
    if Path::new("/opt/homebrew/bin").is_dir() {
        "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_string()
    } else {
        "/usr/bin:/bin:/usr/sbin:/sbin".to_string()
    }
}

pub(super) fn bash_tool(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
) -> Result<ToolOutput, String> {
    bash_tool_streaming(ws, args, timeout_secs, &|_| {}, &AtomicBool::new(false))
}

/// Executes bash with bounded live chunks, a full artifact, and cooperative cancellation.
/// The callback runs before the final result, on the calling thread, with at most 16 KiB.
/// Cancellation is checked at least every pipe-drain pass (maximum 256 KiB / 20 ms poll).
pub fn bash_tool_streaming(
    ws: &Workspace,
    args: &Value,
    timeout_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    let command = req_str(args, "command")?;
    if command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    if command.len() > MAX_COMMAND {
        return Err(format!("command is longer than {MAX_COMMAND} bytes"));
    }
    if timeout_secs == 0 {
        return Err("the harness bash timeout must be positive".to_string());
    }
    let timeout_secs = match opt_u64(args, "timeout")? {
        Some(0) => return Err("timeout must be at least 1 second".to_string()),
        Some(requested) => requested.min(timeout_secs),
        None => timeout_secs,
    };

    if cancel.load(Ordering::Acquire) {
        return Ok(ToolOutput::err("[cancelled before command started]\n"));
    }
    let mut spool = ws
        .artifacts()
        .map(|store| store.spool())
        .transpose()
        .map_err(|e| format!("creating command output artifact: {e}"))?;
    let mut spawn = shell_command(ws, command, &ws.root)?;
    let (mut reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
    let writer_err = writer
        .try_clone()
        .map_err(|e| format!("creating output pipe: {e}"))?;
    spawn
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer_err)
        .process_group(0);
    let child = spawn
        .spawn()
        .map_err(|e| format!("starting sandboxed bash: {e}"))?;
    // The Command owns the pipe's write ends; release them so EOF is observable.
    drop(spawn);

    let mut group = ProcessGroup::new(child);
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(format!(
            "configuring output pipe: {}",
            io::Error::last_os_error()
        ));
    }

    let timeout = Duration::from_secs(timeout_secs);
    let started = Instant::now();
    let mut capture = Capture::new();
    let mut eof = false;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            break;
        }
        if !eof {
            eof = drain_pipe(&mut reader, &mut capture, &mut spool, on_output)
                .map_err(|e| format!("capturing command output: {e}"))?;
        }
        if group
            .exited()
            .map_err(|e| format!("observing command exit: {e}"))?
        {
            break;
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            break;
        }
        if eof {
            std::thread::sleep(Duration::from_millis(5));
        } else {
            wait_readable(fd, 20);
        }
    }
    group.terminate();
    let status = group
        .child
        .wait()
        .map_err(|e| format!("reaping command: {e}"))?;
    // Everything in the group is dead; collect what it wrote. A process that
    // escaped the group may still hold the pipe, so the wait is bounded.
    let grace = Instant::now();
    while !eof && grace.elapsed() < Duration::from_millis(300) {
        eof = drain_pipe(&mut reader, &mut capture, &mut spool, on_output)
            .map_err(|e| format!("capturing command output: {e}"))?;
        if !eof {
            wait_readable(fd, 20);
        }
    }

    let mut text = capture.finish();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let success = status.success() && !timed_out && !cancelled;
    if cancelled {
        text.push_str("[cancelled; process group killed]\n");
    } else if timed_out {
        text.push_str(&format!(
            "[timed out after {timeout_secs}s; process group killed]\n"
        ));
    } else if let Some(code) = status.code() {
        text.push_str(&format!("[exit code: {code}]\n"));
    } else if let Some(signal) = status.signal() {
        text.push_str(&format!("[terminated by signal {signal}]\n"));
    }
    if !success && text.contains("Operation not permitted") && text.contains("/tmp/") {
        text.push_str("[sandbox: /tmp is outside the project and not writable; use $TMPDIR, a scratch directory inside the project]\n");
    }
    if let Some(spool) = spool {
        let uri = spool
            .finish()
            .map_err(|e| format!("saving complete command output artifact: {e}"))?;
        text.push_str(&format!("[complete output: {uri}]\n"));
    }
    Ok(ToolOutput {
        text,
        is_error: !success,
    })
}

/// The command that runs `script` under `/bin/bash -c` in `cwd`, confined exactly as the `bash`
/// tool is: the workspace sandbox policy, or the explicit unsandboxed opt-out, with a scrubbed
/// environment. Fails closed when no sandbox is available. Stdio and process group are the
/// caller's; `cwd` must already be validated to lie inside the workspace.
pub(super) fn shell_command(
    ws: &Workspace,
    script: &str,
    cwd: &Path,
) -> Result<std::process::Command, String> {
    let root = ws.root.to_str().ok_or("workspace path is not UTF-8")?;
    let cwd_str = cwd.to_str().ok_or("working directory is not UTF-8")?;
    let scratch = ws.root.join(SCRATCH_DIR);
    fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
    let scratch_str = scratch
        .to_str()
        .ok_or("scratch path is not UTF-8")?
        .to_owned();
    let strict = matches!(ws.policy, None | Some(Policy::Strict { .. }));
    let mut spawn;
    if ws.unsandboxed {
        spawn = std::process::Command::new("/bin/bash");
        spawn.args(["-c", script]).env_clear();
        for (key, value) in std::env::vars() {
            if !is_secret_env(&key, &value) {
                spawn.env(key, value);
            }
        }
        spawn
            .env("TMPDIR", &scratch_str)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat");
    } else {
        let sandbox = ws.sandbox.clone().unwrap_or_else(Sandbox::detect);
        spawn = match &ws.policy {
            Some(policy) => {
                sandbox.command_with_policy(&ws.root, policy, "/bin/bash", &["-c", script])?
            }
            None => sandbox.command(&ws.root, ws.allow_network, "/bin/bash", &["-c", script])?,
        };
        if strict {
            // The strict sandbox leaves the environment to us: minimal and secret-free.
            spawn
                .env_clear()
                .env("PATH", bash_path())
                .env("HOME", root)
                .env("TMPDIR", &scratch_str)
                .env("PWD", cwd_str)
                .env("SHELL", "/bin/bash")
                .env("LANG", "en_US.UTF-8");
            if Path::new(CLT_DIR).is_dir() {
                // Lets the /usr/bin xcrun shims (git, python3, …) run without Xcode.app paths.
                spawn.env("DEVELOPER_DIR", CLT_DIR);
            }
        } else {
            // Developer sandbox already scrubbed secrets; just make output non-interactive.
            spawn.env("PAGER", "cat").env("GIT_PAGER", "cat");
        }
    }
    spawn.current_dir(cwd).env("TERM", "dumb");
    Ok(spawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::execute;
    use crate::tools::testkit::*;
    use serde_json::json;
    use std::path::PathBuf;
    fn sandboxed() -> bool {
        super::super::sandbox::require_for_test()
    }

    fn bash(ws: &Workspace, command: &str) -> ToolOutput {
        run(ws, "bash", json!({"command": command}))
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
                    "OLDPWD"
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
        let store = super::super::artifact::ArtifactStore::new(artifacts.path()).unwrap();
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
    }

    // Run in its own test process: installing SIGINT must not affect concurrent tests.
    #[test]
    #[ignore]
    fn sigint_shell_helper() {
        let root = std::env::var("DIAL_TEST_SHELL_SIGINT_ROOT")
            .expect("launched by SIGINT regression test");
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

    #[test]
    fn sigint_cancellation_kills_shell_and_process_group_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::shell::tests::sigint_shell_helper",
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
}
