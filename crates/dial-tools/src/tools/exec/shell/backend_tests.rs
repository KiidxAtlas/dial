use super::*;

fn bash() -> PathBuf {
    PathBuf::from("/bin/bash")
}

#[test]
fn explicit_system_uses_bash_or_fails_clearly() {
    assert_eq!(
        resolve_runner(ShellBackend::System, Some(bash()), true, false),
        Ok(Runner::Bash(bash()))
    );
    let err = resolve_runner(ShellBackend::System, None, true, false).unwrap_err();
    assert!(err.contains("no bash"), "{err}");
    assert!(err.contains("\"embedded\""), "{err}");
}

#[test]
fn explicit_embedded_ignores_an_installed_bash() {
    assert_eq!(
        resolve_runner(ShellBackend::Embedded, Some(bash()), true, false),
        Ok(Runner::Embedded)
    );
}

#[test]
fn embedded_without_the_feature_is_a_clear_error() {
    let err = resolve_runner(ShellBackend::Embedded, Some(bash()), false, false).unwrap_err();
    assert!(err.contains("embedded-shell"), "{err}");
    assert!(err.contains("\"system\""), "{err}");
}

#[test]
fn powershell_is_windows_only() {
    assert_eq!(
        resolve_runner(ShellBackend::PowerShell, None, true, true),
        Ok(Runner::PowerShell)
    );
    let err = resolve_runner(ShellBackend::PowerShell, Some(bash()), true, false).unwrap_err();
    assert!(err.contains("only available on Windows"), "{err}");
}

#[test]
fn auto_prefers_bash_then_embedded() {
    assert_eq!(
        resolve_runner(ShellBackend::Auto, Some(bash()), true, false),
        Ok(Runner::Bash(bash()))
    );
    assert_eq!(
        resolve_runner(ShellBackend::Auto, None, true, false),
        Ok(Runner::Embedded)
    );
}

#[test]
fn auto_on_windows_prefers_git_bash_then_embedded_then_powershell() {
    let git = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
    assert_eq!(
        resolve_runner(ShellBackend::Auto, Some(git.clone()), true, true),
        Ok(Runner::Bash(git))
    );
    assert_eq!(
        resolve_runner(ShellBackend::Auto, None, true, true),
        Ok(Runner::Embedded)
    );
    assert_eq!(
        resolve_runner(ShellBackend::Auto, None, false, true),
        Ok(Runner::PowerShell)
    );
}

#[test]
fn auto_without_any_shell_is_a_clear_error() {
    let err = resolve_runner(ShellBackend::Auto, None, false, false).unwrap_err();
    assert!(err.contains("no bash was found"), "{err}");
}

/// Creates `path` with empty contents, along with its parent folders.
fn touch(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"").unwrap();
}

#[test]
fn git_bash_needs_bash_and_git_from_the_same_installation() {
    let root = std::env::temp_dir().join(format!("dial-git-bash-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let installed = root.join("Git");
    touch(&installed.join("bin").join("bash.exe"));
    touch(&installed.join("cmd").join("git.exe"));
    // WSL's launcher sits in System32 with no Git installation beside it.
    touch(&root.join("System32").join("bash.exe"));
    // A bin folder whose installation lacks git.exe is not Git for Windows.
    touch(&root.join("Other").join("bin").join("bash.exe"));

    let found = git_for_windows_bash([
        root.join("System32"),
        root.join("Other").join("bin"),
        installed.join("bin"),
    ]);
    assert_eq!(found, Some(installed.join("bin").join("bash.exe")));
    assert_eq!(
        git_for_windows_bash([root.join("System32"), root.join("Other").join("bin")]),
        None
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn inprocess_resolves_ignoring_installed_bash() {
    assert_eq!(
        resolve_runner(ShellBackend::InProcess, Some(bash()), true, false),
        Ok(Runner::InProcess)
    );
}

#[test]
fn inprocess_without_the_feature_is_a_clear_error() {
    let err = resolve_runner(ShellBackend::InProcess, Some(bash()), false, false).unwrap_err();
    assert!(err.contains("embedded-shell"), "{err}");
}

#[test]
fn inprocess_config_name_round_trips() {
    let backend: ShellBackend = serde_json::from_str("\"inprocess\"").unwrap();
    assert_eq!(backend, ShellBackend::InProcess);
    assert_eq!(serde_json::to_string(&backend).unwrap(), "\"inprocess\"");
}

#[test]
fn configured_zero_cap_means_no_deadline_and_explicit_timeouts_are_capped() {
    assert_eq!(effective_timeout(None, 0), 0);
    assert_eq!(effective_timeout(None, 60), 60);
    assert_eq!(effective_timeout(None, 900), DEFAULT_BASH_TIMEOUT_SECS);
    assert_eq!(effective_timeout(Some(0), 0), 0);
    assert_eq!(effective_timeout(Some(0), 120), 120);
    assert_eq!(effective_timeout(Some(1000), 0), 1000);
    assert_eq!(effective_timeout(Some(1000), 120), 120);
}
