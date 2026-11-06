use crate::config::types::ShellBackend;
use crate::tools::exec::shell::shell_command;
use crate::tools::testkit::workspace;
use std::os::unix::fs::PermissionsExt;

#[test]
fn missing_python_reports_only_an_executable_in_the_current_shell_path() {
    // PATH is changed *inside* the script, and its relative directory is resolved after cd.
    // The alternative would leave a marker if the recovery accidentally executed it.
    for executable in [true, false] {
        let (dir, ws) = workspace();
        let ws = ws
            .without_sandbox()
            .with_shell_backend(ShellBackend::System);
        let bin = dir.path().join("nested/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let alternative = bin.join("python3");
        std::fs::write(&alternative, "#!/bin/sh\nprintf executed > marker\n").unwrap();
        std::fs::set_permissions(
            &alternative,
            std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .unwrap();
        let output = shell_command(
            &ws,
            "cd nested; export PATH=bin; python --version",
            ws.root(),
        )
        .unwrap()
        .env_remove("BASH_ENV")
        .output()
        .unwrap();
        assert_eq!(output.status.code(), Some(127));
        let text = String::from_utf8(output.stderr).unwrap();
        assert!(text.contains("python: command not found"), "{text}");
        let expected = if executable {
            "available alternative in this shell PATH: python3; compatibility not checked"
        } else {
            "checked alternative: python3; not available in this shell PATH"
        };
        assert!(text.contains(expected), "{text}");
        assert!(!dir.path().join("nested/marker").exists());
    }
}

#[test]
fn no_alternative_success_and_remote_diagnostics_do_not_invent_availability() {
    let (_dir, ws) = workspace();
    let ws = ws
        .without_sandbox()
        .with_shell_backend(ShellBackend::System);
    for (script, status, expected) in [
        (
            "export PATH=/nonexistent; python",
            127,
            "[missing program: python; checked alternative: python3; not available in this shell PATH]\n",
        ),
        (
            "set -e; export PATH=/nonexistent; python",
            127,
            "[missing program: python; checked alternative: python3; not available in this shell PATH]\n",
        ),
        ("printf success", 0, "success"),
        (
            "export PATH=/nonexistent; printf 'python: command not found\\n' >&2; exit 127",
            127,
            "python: command not found\n",
        ),
        (
            "export PATH=/nonexistent; dial_unknown_program",
            127,
            "dial_unknown_program: command not found\n",
        ),
        ("export PATH=/nonexistent; (exit 127)", 127, ""),
        ("python() { return 127; }; python", 127, ""),
    ] {
        let output = shell_command(&ws, script, ws.root())
            .unwrap()
            .env_remove("BASH_ENV")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(status), "{script}");
        let text = if status == 0 {
            output.stdout
        } else {
            output.stderr
        };
        let text = String::from_utf8(text).unwrap();
        if script.ends_with("; python") || script.ends_with("; dial_unknown_program") {
            assert!(text.ends_with(expected), "{script}: {text}");
        } else {
            assert_eq!(text, expected, "{script}");
        }
    }
}

#[test]
fn an_operator_command_not_found_handler_is_preserved() {
    let (dir, ws) = workspace();
    let ws = ws
        .without_sandbox()
        .with_shell_backend(ShellBackend::System);
    let env_file = dir.path().join("bash-env");
    std::fs::write(
        &env_file,
        "command_not_found_handle() { printf 'operator handler\\n' >&2; return 42; }\n",
    )
    .unwrap();
    let output = shell_command(&ws, "command_not_found_handle", ws.root())
        .unwrap()
        .env("BASH_ENV", env_file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert_eq!(output.stderr, b"operator handler\n");
}

#[test]
fn an_operator_err_trap_is_preserved() {
    let (dir, ws) = workspace();
    let ws = ws
        .without_sandbox()
        .with_shell_backend(ShellBackend::System);
    let env_file = dir.path().join("bash-env");
    std::fs::write(&env_file, "trap 'printf operator-trap >&2' ERR\n").unwrap();
    let output = shell_command(
        &ws,
        "export PATH=/nonexistent; dial_unknown_program",
        ws.root(),
    )
    .unwrap()
    .env("BASH_ENV", env_file)
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(127));
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.ends_with("operator-trap"), "{text}");
    assert!(!text.contains("[missing program:"), "{text}");
}
