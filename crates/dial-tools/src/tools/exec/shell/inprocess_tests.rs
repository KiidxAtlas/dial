use super::*;
use crate::tools::testkit::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;

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
    let ws = ws.with_cargo_session("session-1");
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
    assert!(!out.is_error, "{}", out.text);
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
    assert!(!out.is_error, "{}", out.text);
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
        assert!(err.contains("sandbox.mode"), "{err}");
        assert!(err.contains("inprocess"), "{err}");
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
    let (dir, ws) = unsandboxed();
    // Environment is process-global; only this test touches these names.
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-inprocess-secret");
        std::env::set_var("DIAL_INPROCESS_PLAIN", "visible");
    }
    fs::create_dir(dir.path().join("sub")).unwrap();
    let out = run_with(
        &ws,
        json!({"command": "echo \"key=[${OPENAI_API_KEY}] plain=[${DIAL_INPROCESS_PLAIN}]\"; pwd", "cwd": "sub"}),
        20,
        &AtomicBool::new(false),
    )
    .unwrap();
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DIAL_INPROCESS_PLAIN");
    }
    assert!(out.text.contains("key=[]"), "{}", out.text);
    assert!(!out.text.contains("sk-inprocess-secret"));
    assert!(out.text.contains("plain=[]") || out.text.contains("plain=[visible]"));
    let sub = dir.path().join("sub").canonicalize().unwrap();
    assert!(
        out.text.contains(sub.to_str().unwrap()) || out.text.contains("/sub\n"),
        "{}",
        out.text
    );
    assert!(!out.is_error, "{}", out.text);
}
