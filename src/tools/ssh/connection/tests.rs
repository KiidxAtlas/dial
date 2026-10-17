use super::*;
use crate::config::tools::ssh::{HostKeyChecking, SshConfig};

fn settings(user_dir: &str) -> Settings {
    Settings {
        user_dir: PathBuf::from(user_dir),
        config: SshConfig::default(),
    }
}

#[test]
fn command_has_batch_mode_control_master_and_target() {
    let settings = settings("/home/u/.dial");
    let target = Target {
        name: "prod".into(),
        host: "10.0.0.5".into(),
        user: Some("deploy".into()),
        port: Some(2222),
        key: None,
        compat: None,
    };
    let args = common_args(
        &target,
        &settings,
        Some(Path::new("/home/u/.dial/ssh/cm")),
        false,
    );
    let joined = args.join(" ");
    assert_eq!(args[0], "-n");
    assert!(joined.contains("-o ControlMaster=auto -o ControlPath=/home/u/.dial/ssh/cm/dial-cm-%C -o ControlPersist=3600"));
    assert!(joined.contains("-o BatchMode=yes"));
    assert!(joined.contains("-o StrictHostKeyChecking=accept-new"));
    assert!(joined.contains("-o ConnectTimeout=15"));
    assert!(joined.ends_with("-p 2222"));
    assert_eq!(target.destination().unwrap(), "deploy@10.0.0.5");
    let streaming = common_args(&target, &settings, None, true);
    assert!(!streaming.contains(&"-n".to_owned()));
    assert!(!streaming.join(" ").contains("ControlPath"));
}

#[test]
fn strict_host_keys_and_spaced_control_paths() {
    let mut settings = settings("/x");
    settings.config.host_key_checking = HostKeyChecking::Yes;
    let args = common_args(
        &Target::opaque("h"),
        &settings,
        Some(Path::new("/a b/100%")),
        false,
    )
    .join(" ");
    assert!(args.contains("StrictHostKeyChecking=yes"));
    assert!(args.contains("ControlPath=\"/a b/100%%/dial-cm-%C\""));
    assert_eq!(
        control_path_option(Path::new("/c")),
        "ControlPath=/c/dial-cm-%C"
    );
}

#[test]
fn destination_refuses_option_injection() {
    assert!(Target::opaque("-oProxyCommand=x").destination().is_err());
    let mut target = Target::opaque("h");
    target.user = Some("-l".into());
    assert!(target.destination().is_err());
}

#[test]
fn control_path_budget_and_fallback() {
    assert!(control_path_fits(Path::new("/Users/me/.dial/ssh/cm"), true));
    let long = PathBuf::from(format!("/{}", "x".repeat(60)));
    assert!(!control_path_fits(&long, true));
    let fallback = fallback_control_dir(&long, 501);
    assert!(control_path_fits(&fallback, true));
    assert_eq!(fallback, fallback_control_dir(&long, 501));
    assert_ne!(fallback, fallback_control_dir(&long, 502));
    assert!(fallback.to_string_lossy().starts_with("/tmp/dial-ssh-"));
    let settings = settings(&format!("/{}", "y".repeat(80)));
    let (dir, shared) = control_dir(&settings);
    assert_eq!(shared, cfg!(unix));
    assert!(control_path_fits(&dir, true) || !cfg!(unix));
}

#[cfg(unix)]
#[test]
fn shared_control_dir_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("cm");
    create_private_dir(&dir).unwrap();
    assert!(check_owner_private(&dir).is_ok());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(check_owner_private(&dir).is_err());
    create_private_dir(&dir).unwrap();
    assert!(check_owner_private(&dir).is_ok());
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    assert!(check_owner_private(&link).is_err());
}

#[test]
fn probe_payload_ignores_banner_noise() {
    let stdout =
        "Welcome to prod\nLast login: yesterday\nDIAL_HOST_PROBE=linux-gnu|/bin/bash|5.2.15\n";
    let payload = extract_probe_payload(stdout, "", HOST_PROBE_MARKER).unwrap();
    assert_eq!(classify_probe(&payload), (Os::Linux, Shell::Bash, true));
    assert_eq!(
        extract_probe_payload(
            "",
            "  DIAL_HOST_PROBE=darwin23|/bin/zsh|",
            HOST_PROBE_MARKER
        )
        .as_deref(),
        Some("darwin23|/bin/zsh|")
    );
    assert_eq!(
        extract_probe_payload("nothing", "", HOST_PROBE_MARKER),
        None
    );
    assert_eq!(
        find_probe_marker("motd DIAL_TRANSFER_OK|Linux\n", "", TRANSFER_PROBE_MARKER).as_deref(),
        Some("Linux\n")
    );
}

#[test]
fn classifies_windows_and_exotic_shells() {
    assert_eq!(
        classify_probe("$OSTYPE|$SHELL|$BASH_VERSION").0,
        Os::Windows
    );
    assert_eq!(
        classify_probe("%OS%|C:\\Windows\\system32\\cmd.exe|"),
        (Os::Windows, Shell::Cmd, false)
    );
    assert_eq!(classify_probe("msys|/usr/bin/bash|5.1").0, Os::Windows);
    assert_eq!(
        classify_probe("darwin22|/usr/local/bin/fish|"),
        (Os::Macos, Shell::Unknown, false)
    );
    assert_eq!(parse_shell("/bin/dash"), Shell::Sh);
    assert_eq!(parse_shell("/usr/bin/tcsh"), Shell::Unknown);
    assert_eq!(parse_shell("pwsh"), Shell::Powershell);
    assert_eq!(os_from_uname("Linux"), Some(Os::Linux));
    assert_eq!(os_from_uname("MINGW64_NT-10.0"), Some(Os::Windows));
    assert_eq!(os_from_uname("FreeBSD"), None);
}

#[test]
fn refresh_and_compat_rules() {
    let target = Target::opaque("w");
    let windows = HostInfo {
        version: HOST_INFO_VERSION,
        os: Os::Windows,
        shell: Shell::Bash,
        transfer_shell: None,
        compat_shell: None,
        compat_enabled: false,
    };
    let applied = apply_compat(&target, windows.clone());
    assert_eq!(applied.compat_shell, Some(PosixShell::Bash));
    assert!(applied.compat_enabled);
    assert_eq!(applied.label(), "windows/bash");
    let mut off = target.clone();
    off.compat = Some(false);
    assert!(!apply_compat(&off, windows).compat_enabled);
    let linux = HostInfo {
        version: HOST_INFO_VERSION,
        os: Os::Linux,
        shell: Shell::Zsh,
        transfer_shell: Some(PosixShell::Sh),
        compat_shell: None,
        compat_enabled: false,
    };
    assert!(!should_refresh(&target, &linux));
    assert!(should_refresh(
        &target,
        &HostInfo {
            transfer_shell: None,
            ..linux.clone()
        }
    ));
    assert!(should_refresh(
        &target,
        &HostInfo {
            version: 0,
            ..linux.clone()
        }
    ));
    assert!(should_refresh(
        &target,
        &HostInfo {
            os: Os::Unknown,
            ..linux
        }
    ));
    assert_eq!(PosixShell::Sh.wrap("echo 'a'"), "sh -c 'echo '\\''a'\\'''");
}
