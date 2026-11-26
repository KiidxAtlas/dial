use super::*;

#[test]
fn computer_error_payload_round_trip() {
    let error = ComputerError::abort("stopped");
    let payload = error.to_payload();
    assert_eq!(ComputerError::from_payload(&payload), error);
}

#[test]
fn computer_parse_null_args_as_empty_object() {
    let parsed: ListApplicationsArgs = parse("listApplications", Value::Null).unwrap();
    assert!(parsed.options.is_none());
}

#[cfg(unix)]
#[test]
fn clipboard_helpers_are_bounded_and_failures_are_classified() {
    let short = std::time::Duration::from_millis(300);
    assert!(matches!(
        run_clipboard("dial-no-such-clipboard-helper", &[], None, short),
        Err(ClipboardFailure::Missing(_))
    ));
    match run_clipboard("sh", &["-c", "echo no display >&2; exit 3"], None, short) {
        Err(ClipboardFailure::Failed(message)) => assert!(message.contains("no display")),
        _ => panic!("a helper that ran and failed is a failure, not a missing helper"),
    }
    let started = std::time::Instant::now();
    assert!(matches!(
        run_clipboard("sh", &["-c", "exec sleep 30"], None, short),
        Err(ClipboardFailure::Failed(message)) if message.contains("did not finish")
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        run_clipboard("sh", &["-c", "printf copied"], None, short).ok(),
        Some("copied".to_owned())
    );
    // A writer that never reads its input is still stopped by the deadline.
    let large = "x".repeat(1 << 20);
    assert!(matches!(
        run_clipboard("sh", &["-c", "exec sleep 30"], Some(&large), short),
        Err(ClipboardFailure::Failed(_))
    ));
}

#[cfg(windows)]
#[test]
fn windows_clipboard_text_transport_is_exact() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // Exercise Windows PowerShell and its redirected pipes, never the user's clipboard.
    let fixture = r"class MemoryClipboard {
        static [string] $value = 'previous contents'
        static [void] Clear() { [MemoryClipboard]::value = '' }
        static [void] SetText([string] $s) { [MemoryClipboard]::value = $s }
        static [string] GetText() { return [MemoryClipboard]::value }
    }
    $ErrorActionPreference = 'Stop'; $clip = [MemoryClipboard];";
    let script = format!("{fixture} {WINDOWS_CLIPBOARD_WRITE}; {WINDOWS_CLIPBOARD_READ}");
    for text in [
        "\u{feff}café🙂\rfirst\nsecond\r\n\r\n",
        "no newline",
        "",
        "a\0b",
    ] {
        let mut child = crate::platform::process::spawn(
            Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Sta", "-Command", &script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        if text.contains('\0') {
            assert!(!output.status.success());
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, text.as_bytes());
        }
    }
}

#[test]
fn delayed_admitted_requests_cannot_cross_lifecycle_boundaries() {
    for method in ["retire", "cancel", "releaseControl"] {
        let session = DesktopSession::new(None).unwrap();
        let delayed = session.admit();
        let (resume, wait) = std::sync::mpsc::channel();
        let old = std::thread::spawn(move || {
            wait.recv().unwrap();
            // Both direct desktop calls and non-worker RPCs must reject stale work.
            assert!(
                delayed
                    .list_displays()
                    .unwrap_err()
                    .message
                    .contains("cancelled")
            );
            dispatch(&delayed, "controlState", Value::Null).unwrap_err()
        });
        dispatch(&session.admit(), method, Value::Null).unwrap();
        let live = session.admit();
        assert_eq!(
            dispatch(&live, "controlState", Value::Null).unwrap()["active"],
            false
        );
        resume.send(()).unwrap();
        let error = old.join().unwrap();
        assert!(error.is_abort, "{method}: {error}");
        drop(live);
        // Dropping an admitted handle must not cancel other live requests.
        assert!(dispatch(&session.admit(), "controlState", Value::Null).is_ok());
    }
}

#[test]
fn menu_select_options_preserve_omission_and_explicit_delivery() {
    for (options, expected) in [
        (Value::Null, None),
        (json!({}), None),
        (json!({"takeover": false}), Some(false)),
        (json!({"takeover": true}), Some(true)),
    ] {
        let parsed: MenuSelectArgs = parse(
            "menuSelect",
            json!({
                "target": "42103", "path": ["File", "New Window"], "options": options
            }),
        )
        .unwrap();
        assert_eq!(parsed.target, "42103");
        assert_eq!(parsed.options.unwrap_or_default().takeover, expected);
    }
    let parsed: MenuSelectArgs = parse(
        "menuSelect",
        json!({
            "target": "42103", "path": ["File", "New Window"]
        }),
    )
    .unwrap();
    assert!(parsed.options.is_none());
    for options in [json!({"takeover": "true"}), json!({"button": "left"})] {
        assert!(
            parse::<MenuSelectArgs>(
                "menuSelect",
                json!({
                    "target": "42103", "path": ["File", "New Window"], "options": options
                })
            )
            .is_err()
        );
    }
}
