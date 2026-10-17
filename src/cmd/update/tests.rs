use super::*;

#[test]
fn checksum_selects_exact_asset_and_normalizes_hash() {
    let manifest = format!(
        "{}  dial-v1-x.tar.gz\n{}  dial-v1-y.tar.gz\n",
        "A".repeat(64),
        "b".repeat(64)
    );
    assert_eq!(
        checksum(&manifest, "dial-v1-x.tar.gz").unwrap(),
        "a".repeat(64)
    );
    assert!(checksum(&manifest, "../dial-v1-x.tar.gz").is_err());
}

#[test]
fn signature_verification_accepts_only_the_signed_manifest() {
    let key = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    let signature =
        "5VZDAMNgrHKQhuLMgG6CioSHfx645dl02HPgZSJJAVVfuIIVkKM7rMYeOXAc+bRr0lv18FlbviRlUUFDjnoQCw==";
    assert!(verify_signature_with_key(b"", signature.as_bytes(), key).is_ok());
    assert!(verify_signature_with_key(b"changed", signature.as_bytes(), key).is_err());
    assert!(verify_signature_with_key(b"", b"not base64!", key).is_err());
}

#[test]
fn installation_detection_preserves_package_manager_ownership() {
    assert_eq!(
        installation_kind(Path::new("/opt/homebrew/Cellar/dial/bin/dial")),
        "Homebrew"
    );
    assert_eq!(
        installation_kind(Path::new("/home/user/.cargo/bin/dial")),
        "Cargo"
    );
    assert_eq!(
        installation_kind(Path::new("/nix/store/hash-dial/bin/dial")),
        "Nix"
    );
    assert_eq!(
        installation_kind(Path::new("/home/user/.local/bin/dial")),
        "script"
    );
    assert_eq!(
        installation_kind(Path::new(r"C:\Users\user\.cargo\bin\dial.exe")),
        "Cargo"
    );
}

#[cfg(unix)]
mod activation_fixtures {
    use super::*;
    use std::path::PathBuf;

    fn accept(_: &Path) -> Result<()> {
        Ok(())
    }

    fn reject(_: &Path) -> Result<()> {
        bail!("post-install check rejected the update")
    }

    /// Active `dial` holding "old", a staged "new" candidate and its staging directory.
    fn fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let staging = root.join("staging");
        fs::create_dir_all(&staging).unwrap();
        let executable = root.join("dial");
        let staged = staging.join("dial.new");
        fs::write(&executable, "old").unwrap();
        fs::write(&staged, "new").unwrap();
        (executable, staged, root.join("dial.bak"), staging)
    }

    #[test]
    fn success_keeps_the_previous_executable_as_a_verified_backup() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        activate_unix(&exe, &staged, &backup, &staging, &accept).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
        assert!(!staged.exists());
    }

    #[test]
    fn failed_post_check_restores_the_previous_executable() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        let error = activate_unix(&exe, &staged, &backup, &staging, &reject).unwrap_err();
        assert!(format!("{error:#}").contains("previous version restored"));
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert!(!backup.exists());
    }

    #[test]
    fn failed_rollback_reports_both_errors_and_keeps_an_active_executable() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        // The rollback point disappears after the swap, so restoring it must fail.
        let sabotage = |path: &Path| -> Result<()> {
            fs::remove_file(&backup)?;
            reject(path)
        };
        let error = activate_unix(&exe, &staged, &backup, &staging, &sabotage).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("post-install check rejected the update"),
            "{message}"
        );
        assert!(
            message.contains("restoring the previous executable also failed"),
            "{message}"
        );
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
    }

    #[test]
    fn a_missing_candidate_leaves_the_active_executable_untouched() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        fs::remove_file(&staged).unwrap();
        assert!(activate_unix(&exe, &staged, &backup, &staging, &accept).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
    }

    #[test]
    fn a_failed_backup_stops_before_the_active_name_changes() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        fs::create_dir_all(backup.join("occupied")).unwrap();
        assert!(activate_unix(&exe, &staged, &backup, &staging, &accept).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert!(
            staged.exists(),
            "the candidate is consumed only after the backup is preserved"
        );
    }

    #[test]
    fn an_interruption_after_backup_leaves_the_original_executable_active() {
        let root = tempfile::tempdir().unwrap();
        let (exe, _staged, backup, staging) = fixture(root.path());
        preserve_backup(&exe, &backup, &staging).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
    }

    #[test]
    fn concurrent_updates_are_serialized_by_the_install_lock() {
        let root = tempfile::tempdir().unwrap();
        let first = lock_install_dir(root.path()).unwrap();
        assert!(
            lock_install_dir(root.path()).is_err(),
            "a second updater must not enter while the first holds the lease"
        );
        drop(first);
        assert!(lock_install_dir(root.path()).is_ok());
    }
}
