use super::*;

#[test]
fn linux_policy_rejects_root_missing_and_secret_overlaps() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().canonicalize().unwrap();
    let strict = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: false,
        extra_write: vec![],
    };
    assert!(Mounts::new(Path::new("/"), &strict).is_err());
    assert!(Mounts::new(&ws.join("missing"), &strict).is_err());
    for denied in [&ws, &ws.join(".env"), Path::new("/etc/hosts")] {
        let policy = Policy::Dev {
            deny_read: vec![denied.to_owned()],
            extra_write: vec![],
            allow_network: false,
        };
        assert!(Mounts::new(&ws, &policy).is_err(), "{denied:?}");
    }
    let policy = Policy::Dev {
        deny_read: vec![],
        extra_write: vec![PathBuf::from("/usr")],
        allow_network: false,
    };
    assert!(Mounts::new(&ws, &policy).is_err());
}

#[test]
fn both_policies_mask_nested_private_trees_and_files() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().canonicalize().unwrap();
    let private = ws.join("state/private");
    fs::create_dir_all(&private).unwrap();
    let secret = ws.join("secret-file");
    fs::write(&secret, "private").unwrap();
    for policy in [
        Policy::Strict {
            deny_read: vec![private.clone(), secret.clone()],
            extra_write: vec![],
            allow_network: false,
        },
        Policy::Dev {
            deny_read: vec![private.clone(), secret.clone()],
            extra_write: vec![],
            allow_network: false,
        },
    ] {
        let mut mounts = Mounts::new(&ws, &policy).unwrap();
        assert!(mounts.write.contains(&ws));
        assert!(mounts.masks.contains(&(private.clone(), true)));
        assert!(mounts.masks.contains(&(secret.clone(), false)));
        assert!(
            mounts
                .grant_program(private.join("program").to_str().unwrap())
                .is_err()
        );
    }
}

#[test]
fn outer_private_mask_covers_absent_nested_state_before_metadata_checks() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().canonicalize().unwrap();
    let private = ws.join(".dial");
    fs::create_dir(&private).unwrap();
    let absent = private.join("projects/not-created/models");
    for policy in [
        Policy::Strict {
            deny_read: vec![absent.clone(), private.clone()],
            extra_write: vec![],
            allow_network: false,
        },
        Policy::Dev {
            deny_read: vec![absent.clone(), private.clone()],
            extra_write: vec![],
            allow_network: false,
        },
    ] {
        let mounts = Mounts::new(&ws, &policy).unwrap();
        assert_eq!(mounts.masks, vec![(private.clone(), true)]);
        assert!(!absent.exists());
    }
}

#[test]
fn absent_top_level_credentials_fail_closed_without_creating_host_paths() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().canonicalize().unwrap();
    let absent = ws.join(".aws");
    let policy = Policy::Dev {
        deny_read: vec![absent.clone()],
        extra_write: vec![],
        allow_network: false,
    };
    let error = Mounts::new(&ws, &policy)
        .err()
        .expect("missing private path must fail closed");
    assert!(error.contains("must already exist"));
    assert!(!absent.exists());
}

#[test]
fn wsl_requires_disabled_config_and_current_interpreter() {
    let kernel = "6.6.87.2-microsoft-standard-WSL2";
    let disabled = "[interop]\nenabled=false\nappendWindowsPath=false\n";
    assert!(windows_interop_safe("generic-linux", "", Some("enabled")));
    assert!(windows_interop_safe(kernel, disabled, None));
    assert!(windows_interop_safe(kernel, disabled, Some("disabled\n")));
    assert!(!windows_interop_safe(
        kernel,
        disabled,
        Some("enabled\nflags: PF\n")
    ));
    assert!(!windows_interop_safe(
        kernel,
        "[interop]\nenabled=true",
        None
    ));
    assert!(!windows_interop_safe(kernel, "", None));
    assert!(!windows_interop_safe(
        kernel,
        "[interop]\nenabled=false\nenabled=true",
        None
    ));
    assert!(!windows_interop_safe(
        kernel,
        "[interop]\nenabled=false\n[interop]\nenabled=false",
        None
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sandbox_enforces_filesystem_and_both_network_modes() {
    // Required, not skipped: a release runner without a working sandbox must fail.
    mechanism(false).unwrap();
    mechanism(true).unwrap();
}

#[test]
fn strict_extra_write_roots_become_writable_mounts() {
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("ws");
    let extra = temp.path().join("extra");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&extra).unwrap();
    let policy = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: false,
        extra_write: vec![extra.clone()],
    };
    let mounts = Mounts::new(&ws, &policy).unwrap();
    assert!(mounts.write.contains(&ws.canonicalize().unwrap()));
    assert!(mounts.write.contains(&extra.canonicalize().unwrap()));
}
