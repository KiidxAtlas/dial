//! Foreground `bash` execution for `shell.backend = "inprocess"`: the built-in shell runs on a
//! thread of the Dial process. `run_bash` reaches this only after `runner_for` verified that the
//! sandbox is off. Output capture, artifacts, timeout and cancellation follow the child-process
//! path; the differences (no automatic backgrounding, no named sessions, no hard kill of a
//! builtin that never yields) are documented in docs/shell.md.

use super::*;
use crate::embedded_shell::inprocess::{Request, Run};

/// The environment the shell gets: Dial's, minus secrets, plus the same non-interactive settings
/// the unsandboxed child-process backends set.
fn environment(scratch: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let (key, value) = (key.into_string().ok()?, value.into_string().ok()?);
            (!is_secret_env(&key, &value)).then_some((key, value))
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
    env
}

pub(super) fn run(
    ws: &Workspace,
    command: &str,
    cwd: &Path,
    timeout_secs: u64,
    on_output: &(dyn Fn(&[u8]) + Sync),
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    let scratch = ws.root.join(SCRATCH_DIR);
    fs::create_dir_all(&scratch).map_err(|e| format!("creating scratch directory: {e}"))?;
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
        env: environment(&scratch),
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
    let stopped = cancelled || timed_out || capture_error.is_some();
    let abandoned = stopped && !shell.terminate();
    // Collect what the script wrote; a stuck descendant may keep the pipe open, so this is bounded.
    let grace = Instant::now();
    while !eof && capture_error.is_none() && grace.elapsed() < Duration::from_millis(300) {
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
        if !eof {
            waiter
                .wait(
                    Some(&reader),
                    None::<&Child>,
                    Some(Duration::from_millis(300).saturating_sub(grace.elapsed())),
                )
                .map_err(|e| format!("waiting for command output: {e}"))?;
        }
    }
    if !eof && capture_error.is_none() {
        capture_error = Some(
            "pipe did not reach EOF within the 300ms cleanup grace; output may be incomplete"
                .to_string(),
        );
    }

    let mut text = capture.finish();
    if text.is_empty() {
        text.push_str("(no output)");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let code = shell.code();
    let success = code == Some(0) && !timed_out && !cancelled;
    if cancelled {
        text.push_str("[cancelled; in-process shell stopped]\n");
    } else if timed_out {
        text.push_str(&format!(
            "[timed out after {timeout_secs}s; in-process shell stopped]\n"
        ));
    } else if let Some(code) = code {
        text.push_str(&format!("[exit code: {code}]\n"));
    }
    if abandoned {
        text.push_str("[in-process shell did not stop (a builtin that never yields cannot be interrupted); it was abandoned and may keep using CPU until Dial exits]\n");
    }
    if let Some(spool) = spool {
        match spool.finish() {
            Ok(uri) => {
                let completeness = if capture_error.is_some() {
                    "partial"
                } else {
                    "complete"
                };
                text.push_str(&format!("[{completeness} output: {uri}]\n"));
            }
            Err(error) => {
                artifact_error = Some(format!("saving complete command output artifact: {error}"));
            }
        }
    }
    if let Some(error) = &capture_error {
        text.push_str(&format!("[output capture error: {error}]\n"));
    }
    if let Some(error) = &artifact_error {
        text.push_str(&format!(
            "[output artifact error: {error}; inline output retained]\n"
        ));
    }
    let output = ToolOutput {
        execution: Default::default(),
        data: None,
        text,
        is_error: !success || capture_error.is_some() || artifact_error.is_some(),
    }
    .with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: code.filter(|_| !timed_out && !cancelled),
        signal: None,
        timed_out,
        cancelled,
        output_complete: capture_error.is_none() && artifact_error.is_none() && eof,
    });
    Ok(if cancelled {
        output
            .with_status(crate::tools::ToolStatus::Aborted)
            .with_effects(crate::tools::surface::outcome::Effects::Unknown)
    } else if timed_out {
        output.with_status(crate::tools::ToolStatus::Timeout)
    } else {
        output
    })
}
