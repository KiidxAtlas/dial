//! Foreground `bash` execution for `shell.backend = "inprocess"`: the built-in shell runs on a
//! thread of the Dial process. `run_bash` reaches this only after `runner_for` verified that the
//! sandbox is off. Output capture, artifacts, timeout and cancellation follow the child-process
//! path; the differences (no automatic backgrounding, no named sessions, no hard kill of a
//! builtin that never yields) are documented in docs/shell.md.

use super::*;
use crate::embedded_shell::inprocess::{Request, Run, Termination};

/// The environment the shell gets: Dial's, minus secrets, plus the same non-interactive settings
/// the unsandboxed child-process backends set.
fn environment(scratch: &Path, cargo_target: Option<&Path>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let (key, value) = (key.into_string().ok()?, value.into_string().ok()?);
            child_env_allowed(&key, &value).then_some((key, value))
        })
        .collect();
    let scratch = scratch.display().to_string();
    let mut set = |key: &str, value: &str| {
        env.retain(|(name, _)| name != key);
        env.push((key.to_owned(), value.to_owned()));
    };
    if cfg!(windows) {
        set("TEMP", &scratch);
        set("TMP", &scratch);
    } else {
        set("TMPDIR", &scratch);
        set("PAGER", "cat");
        set("GIT_PAGER", "cat");
        set("TERM", "dumb");
        set("GIT_ASKPASS", "/usr/bin/false");
        set("SSH_ASKPASS", "/usr/bin/false");
    }
    set("GIT_TERMINAL_PROMPT", "0");
    set("GCM_INTERACTIVE", "never");
    if std::env::var_os("CARGO_TARGET_DIR").is_none()
        && let Some(target) = cargo_target
    {
        crate::tools::exec::cargo_target::mark_used(target);
        set("CARGO_TARGET_DIR", &target.to_string_lossy());
    }
    env
}

pub(super) fn run(
    ws: &Workspace,
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    expected_codes: &[i32],
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    let scratch = ws.root.join(SCRATCH_DIR);
    fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
    let entries_before = super::directory_entries(cwd);
    let mut spool = ws
        .artifacts()
        .map(|store| store.spool())
        .transpose()
        .map_err(|e| format!("creating command output artifact: {e}"))?;
    let (mut reader, writer) = io::pipe().map_err(|e| format!("creating output pipe: {e}"))?;
    crate::platform::io::set_nonblocking(&reader)
        .map_err(|e| format!("configuring output pipe: {e}"))?;
    let mut shell = Run::start(Request {
        script: command.to_owned(),
        cwd: cwd.to_owned(),
        env: environment(&scratch, ws.cargo_target_dir().as_deref()),
        output: writer,
    })?;

    let timeout = Duration::from_secs(timeout_secs);
    let started = Instant::now();
    let mut capture = Capture::new();
    let mut artifact_error = None;
    let mut capture_error = None;
    let mut eof = false;
    let mut timed_out = false;
    let mut cancelled = false;
    let waiter = crate::platform::wake::Waiter::new()
        .map_err(|e| format!("creating command wait channel: {e}"))?;
    loop {
        if cancel.load(Ordering::Acquire) {
            cancelled = true;
            break;
        }
        if !eof {
            match drain_pipe(
                &mut reader,
                &mut capture,
                &mut spool,
                &mut artifact_error,
                on_output,
            ) {
                Ok(closed) => eof = closed,
                Err(error) => {
                    capture_error = Some(format!("reading command output: {error}"));
                    break;
                }
            }
        }
        if shell.exited() {
            break;
        }
        if !timeout.is_zero() && started.elapsed() >= timeout {
            timed_out = true;
            break;
        }
        let mut limit = Duration::from_millis(100);
        if !timeout.is_zero() {
            limit = limit.min(timeout.saturating_sub(started.elapsed()));
        }
        waiter
            .wait((!eof).then_some(&reader), None::<&Child>, Some(limit))
            .map_err(|e| format!("waiting for command output: {e}"))?;
    }
    let termination = shell.terminate();
    // Collect what the script wrote; a stuck descendant may keep the pipe open, so this is bounded.
    drain_after_exit(
        &mut reader,
        &mut capture,
        &mut spool,
        &mut artifact_error,
        &mut capture_error,
        &mut eof,
        &waiter,
        on_output,
        Duration::from_millis(300),
    )?;

    let original = capture.total as u64;
    let inline_retained = (capture.head.len() + capture.tail.len().min(BASH_TAIL)) as u64;
    let elided = spool.as_ref().map(|spool| spool.elided_bytes());
    let mut text = captured_text(capture);
    let body_len = text.len();
    let code = shell.code();
    let success =
        code.is_some_and(|code| expected_codes.contains(&code)) && !timed_out && !cancelled;
    append_stop_notes(&mut text, cancelled, timed_out, timeout_secs, &termination);
    if !cancelled
        && !timed_out
        && let Some(code) = code
    {
        append_exit_note(&mut text, command, code, expected_codes);
    }
    if !cancelled && let Some(note) = super::created_entries_note(entries_before.as_ref(), cwd) {
        text.push_str(&note);
    }
    let artifact = append_output_notes(&mut text, spool, &capture_error, &mut artifact_error);
    let minimized = minimize_body(
        ws,
        command,
        body_len,
        &mut text,
        eof && capture_error.is_none() && original == inline_retained,
        None,
    );
    let integrity = output_integrity(
        original,
        inline_retained,
        elided,
        artifact,
        eof && capture_error.is_none(),
        minimized,
    );
    let output = ToolOutput {
        execution: Default::default(),
        data: None,
        text,
        is_error: !success
            || capture_error.is_some()
            || artifact_error.is_some()
            || termination.uncertain(),
    }
    .with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: code.filter(|_| !timed_out && !cancelled),
        signal: None,
        timed_out,
        cancelled,
        output_complete: capture_error.is_none()
            && artifact_error.is_none()
            && eof
            && integrity.retention == crate::runtime::execution::Completeness::Complete,
    })
    .with_output_integrity(integrity)
    .with_shell_launch(crate::runtime::execution::ShellLaunch {
        stdin: crate::runtime::execution::ShellStdin::Eof,
        terminal_allocated: false,
        job: None,
    });
    Ok(finish_stop(output, cancelled, timed_out, &termination))
}

pub(super) fn append_stop_notes(
    text: &mut String,
    cancelled: bool,
    timed_out: bool,
    timeout_secs: u64,
    termination: &Termination,
) {
    let state = if termination.shell_stopped {
        "stopped"
    } else {
        "not stopped"
    };
    if cancelled {
        text.push_str(&format!("[cancelled; in-process shell {state}]\n"));
    } else if timed_out {
        text.push_str(&format!(
            "[timed out after {timeout_secs}s; in-process shell {state}]\n"
        ));
    }
    if !termination.shell_stopped {
        text.push_str("[in-process shell did not stop (a builtin that never yields cannot be interrupted); it was abandoned and may keep using CPU until Dial exits]\n");
    }
    if let Some(error) = &termination.cleanup_error {
        text.push_str(&format!("[external-command cleanup could not be established: {error}; processes may still be running]\n"));
    }
}

pub(super) fn finish_stop(
    mut output: ToolOutput,
    cancelled: bool,
    timed_out: bool,
    termination: &Termination,
) -> ToolOutput {
    if cancelled {
        output = output.with_status(crate::tools::ToolStatus::Aborted);
    } else if timed_out {
        output = output.with_status(crate::tools::ToolStatus::Timeout);
    }
    if cancelled || termination.uncertain() {
        output = output.with_effects(crate::tools::surface::outcome::Effects::Unknown);
    }
    output
}
