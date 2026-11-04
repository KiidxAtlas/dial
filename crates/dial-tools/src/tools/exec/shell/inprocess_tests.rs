use super::*;
use crate::tools::testkit::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[cfg(feature = "embedded-shell")]
fn unsandboxed() -> (tempfile::TempDir, Workspace) {
    let (dir, ws) = workspace();
    (
        dir,
        ws.without_sandbox()
            .with_shell_backend(ShellBackend::InProcess),
    )
}

fn run_with(
    ws: &Workspace,
    args: Value,
    timeout: u64,
    cancel: &AtomicBool,
) -> Result<ToolOutput, String> {
    bash_tool_streaming(ws, &args, timeout, &|_| {}, cancel)
}

/// Script success and bounded cleanup are independent. Real discovery can legitimately exhaust
/// its deadline under parallel test load; accept that only with the complete uncertainty contract.
pub(super) fn assert_zero_exit_with_cleanup_accounted_for(out: &ToolOutput) {
    use crate::tools::surface::outcome::Effects;
    let process = out
        .execution
        .process
        .as_ref()
        .expect("missing process facts");
    assert!(process.started, "{}", out.text);
    assert_eq!(process.exit_code, Some(0), "{}", out.text);
    assert!(!process.cancelled && !process.timed_out, "{}", out.text);
    assert!(process.output_complete, "{}", out.text);
    if out.execution.effects == Effects::Unknown {
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text
                .contains("[external-command cleanup could not be established:")
                && out.text.contains("processes may still be running]"),
            "{}",
            out.text
        );
    } else {
        assert_eq!(out.execution.effects, Effects::Known, "{}", out.text);
        assert!(!out.is_error, "{}", out.text);
        assert!(
            !out.text.contains("cleanup could not be established"),
            "{}",
            out.text
        );
    }
}

#[cfg(feature = "embedded-shell")]
#[test]
fn runs_in_process_when_sandbox_is_off() {
    let (_dir, ws) = unsandboxed();
    let out = run_with(
        &ws,
        json!({"command": "echo hi; echo \"$$\" > pid.txt; exit 3"}),
        20,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(out.text.starts_with("hi\n"), "{}", out.text);
    assert!(out.text.contains("[exit code: 3]"), "{}", out.text);
    assert!(out.is_error);
    // The shell reports Dial's own pid: no child host process was started.
    let pid = fs::read_to_string(ws.root().join("pid.txt")).unwrap();
    assert_eq!(pid.trim(), std::process::id().to_string());
}

#[cfg(feature = "embedded-shell")]
#[test]
fn cargo_session_directory_is_exported_without_overriding_shell_assignments() {
    let (_dir, ws) = unsandboxed();
    let ws = ws.with_cargo_session("session-1").unwrap();
    let inherited = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ws.cargo_target_dir().unwrap());
    let out = run_with(
        &ws,
        json!({"command":"printf '%s' \"$CARGO_TARGET_DIR\""}),
        20,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_zero_exit_with_cleanup_accounted_for(&out);
    assert!(
        out.text
            .starts_with(&inherited.to_string_lossy().to_string()),
        "{}",
        out.text
    );
    let out = run_with(
        &ws,
        json!({"command":"export CARGO_TARGET_DIR=explicit; printf '%s' \"$CARGO_TARGET_DIR\""}),
        20,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_zero_exit_with_cleanup_accounted_for(&out);
    assert!(out.text.starts_with("explicit"), "{}", out.text);
}

#[test]
fn refuses_every_sandbox_mode_other_than_off_and_runs_nothing() {
    use crate::tools::exec::sandbox::Policy;
    let (dir, ws) = workspace();
    let strict = Policy::Strict {
        allow_network: false,
        deny_read: Vec::new(),
        extra_write: Vec::new(),
    };
    for ws in [
        ws.clone().with_shell_backend(ShellBackend::InProcess),
        ws.clone()
            .with_policy(strict)
            .with_shell_backend(ShellBackend::InProcess),
    ] {
        let err = run_with(
            &ws,
            json!({"command": "touch should-not-exist"}),
            20,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        if cfg!(feature = "embedded-shell") {
            assert!(err.contains("sandbox.mode"), "{err}");
            assert!(err.contains("inprocess"), "{err}");
        } else {
            assert!(err.contains("embedded-shell feature is disabled"), "{err}");
        }
        assert!(!dir.path().join("should-not-exist").exists());
    }
}

#[cfg(feature = "embedded-shell")]
#[test]
fn timeout_stops_the_command_and_its_children() {
    let (dir, ws) = unsandboxed();
    let started = Instant::now();
    let out = run_with(
        &ws,
        json!({"command": "sleep 30; touch survived"}),
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(out.text.contains("timed out after 1s"), "{}", out.text);
    assert!(out.is_error);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!dir.path().join("survived").exists());
}

#[cfg(feature = "embedded-shell")]
#[test]
fn cancel_stops_the_command() {
    let (_dir, ws) = unsandboxed();
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let out = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            cancel.store(true, Ordering::Release);
        });
        run_with(&ws, json!({"command": "echo up; sleep 30"}), 60, &cancel).unwrap()
    });
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(out.text.contains("cancelled"), "{}", out.text);
    assert!(out.is_error);
}

#[cfg(feature = "embedded-shell")]
#[test]
fn environment_drops_secrets_and_cwd_is_honoured() {
    if !in_env_subprocess(
        "tools::exec::shell::inprocess_tests::environment_drops_secrets_and_cwd_is_honoured",
        &[
            ("OPENAI_API_KEY", "sk-inprocess-secret"),
            ("DIAL_INPROCESS_PLAIN", "visible"),
        ],
    ) {
        return;
    }
    let (dir, ws) = unsandboxed();
    fs::create_dir(dir.path().join("sub")).unwrap();
    let out = run_with(
        &ws,
        json!({"command": "echo \"key=[${OPENAI_API_KEY}] plain=[${DIAL_INPROCESS_PLAIN}]\"; pwd", "cwd": "sub"}),
        20,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(out.text.contains("key=[]"), "{}", out.text);
    assert!(!out.text.contains("sk-inprocess-secret"));
    assert!(out.text.contains("plain=[]") || out.text.contains("plain=[visible]"));
    let sub = dir.path().join("sub").canonicalize().unwrap();
    assert!(
        out.text.contains(sub.to_str().unwrap()) || out.text.contains("/sub\n"),
        "{}",
        out.text
    );
    assert_zero_exit_with_cleanup_accounted_for(&out);
}

#[test]
fn stop_facts_and_diagnostics_distinguish_abandonment_and_cleanup_failure() {
    use crate::embedded_shell::inprocess::Termination;
    use crate::tools::surface::outcome::Effects;
    for (shell_stopped, cleanup_error, cancelled, timed_out, expected) in [
        (false, None, false, true, Effects::Unknown),
        (
            true,
            Some("discovery failed".to_owned()),
            false,
            true,
            Effects::Unknown,
        ),
        (false, None, true, false, Effects::Unknown),
        (true, None, true, false, Effects::Unknown),
        (true, None, false, true, Effects::Known),
    ] {
        let termination = Termination {
            shell_stopped,
            cleanup_error,
        };
        let output = inprocess::finish_stop(
            ToolOutput {
                execution: Default::default(),
                data: None,
                text: String::new(),
                is_error: true,
            },
            cancelled,
            timed_out,
            &termination,
        );
        assert_eq!(output.execution.effects, expected, "{termination:?}");
        let mut notes = String::new();
        inprocess::append_stop_notes(&mut notes, cancelled, timed_out, 1, &termination);
        assert_eq!(notes.contains("shell stopped]"), shell_stopped, "{notes}");
        assert_eq!(notes.contains("was abandoned"), !shell_stopped, "{notes}");
        assert_eq!(
            notes.contains("processes may still be running"),
            termination.cleanup_error.is_some(),
            "{notes}"
        );
    }
}

#[test]
fn completed_script_keeps_exit_facts_when_cleanup_is_uncertain() {
    use crate::embedded_shell::inprocess::Termination;
    use crate::runtime::execution::ProcessOutcome;
    for cleanup_error in [
        None,
        Some("external-command cleanup deadline exceeded".to_owned()),
    ] {
        let termination = Termination {
            shell_stopped: true,
            cleanup_error,
        };
        let mut text = "script output\n".to_owned();
        inprocess::append_stop_notes(&mut text, false, false, 1, &termination);
        let output = inprocess::finish_stop(
            ToolOutput {
                execution: Default::default(),
                data: None,
                text,
                is_error: termination.uncertain(),
            }
            .with_process(ProcessOutcome {
                started: true,
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                cancelled: false,
                output_complete: true,
            }),
            false,
            false,
            &termination,
        );
        assert_zero_exit_with_cleanup_accounted_for(&output);
        assert_eq!(output.is_error, termination.uncertain());
    }
}
