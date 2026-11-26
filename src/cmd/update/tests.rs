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
fn a_source_build_is_current_only_when_clean_and_built_from_head() {
    let head = "0123456789abcdef0123456789abcdef01234567";
    assert!(built_from(head, "0123456789ab", "false"));
    assert!(built_from(head, head, "false"));
    assert!(
        !built_from(head, "0123456789ab", "true"),
        "dirty builds rebuild"
    );
    assert!(!built_from(head, "0123456789ab", "unknown"));
    assert!(!built_from(head, "fedcba987654", "false"), "older commit");
    assert!(!built_from(head, "unknown", "false"));
    assert!(!built_from(head, "", "false"));
}

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
        fs::write(&backup, "older").unwrap();
        activate(&exe, &staged, &backup, &staging, &accept).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
        assert!(!staged.exists());
    }

    #[test]
    fn failed_post_check_restores_the_previous_executable() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        let error = activate(&exe, &staged, &backup, &staging, &reject).unwrap_err();
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
        let error = activate(&exe, &staged, &backup, &staging, &sabotage).unwrap_err();
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
        assert!(activate(&exe, &staged, &backup, &staging, &accept).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        #[cfg(unix)]
        assert_eq!(fs::read_to_string(&backup).unwrap(), "old");
    }

    #[test]
    fn a_failed_backup_stops_before_the_active_name_changes() {
        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        fs::create_dir_all(backup.join("occupied")).unwrap();
        assert!(activate(&exe, &staged, &backup, &staging, &accept).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert!(
            staged.exists(),
            "the candidate is consumed only after the backup is preserved"
        );
    }

    #[cfg(unix)]
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

    #[cfg(windows)]
    #[test]
    fn a_deny_delete_handle_blocks_replacement_without_losing_the_install() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let root = tempfile::tempdir().unwrap();
        let (exe, staged, backup, staging) = fixture(root.path());
        let _busy = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&exe)
            .unwrap();
        assert!(activate(&exe, &staged, &backup, &staging, &accept).is_err());
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
    }

    #[cfg(windows)]
    #[test]
    fn a_partial_replace_restores_the_displaced_image_without_clobbering_a_new_owner() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;
        for occupied in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (exe, _, backup, _) = fixture(root.path());
            fs::rename(&exe, &backup).unwrap();
            if occupied {
                fs::write(&exe, "another installer").unwrap();
            }
            let os_error =
                std::io::Error::from_raw_os_error(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32);
            let error = windows::recover_failed_replace(os_error, &exe, &backup).unwrap_err();
            if occupied {
                assert!(error.to_string().contains("restoring it also failed"));
                assert_eq!(fs::read_to_string(exe).unwrap(), "another installer");
                assert_eq!(fs::read_to_string(backup).unwrap(), "old");
            } else {
                assert!(error.to_string().contains("previous version restored"));
                assert_eq!(fs::read_to_string(exe).unwrap(), "old");
                assert!(!backup.exists());
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn replacing_a_running_image_keeps_the_process_alive_and_publishes_the_candidate() {
        use std::io::Write;
        use std::process::Stdio;

        let root = tempfile::tempdir().unwrap();
        let (path, staged, backup, staging) = fixture(root.path());
        let exe = path.with_extension("exe");
        let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
        fs::copy(&shell, &exe).unwrap();
        // A temporary copy, not the actual system shell. Keep its image mapped until the swap
        // completes; piped stdin also ensures EOF if the test unwinds.
        let mut old = Command::new(&exe)
            .arg("/d")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = activate(&exe, &staged, &backup, &staging, &accept);
        old.stdin.take().unwrap().write_all(b"exit 17\r\n").unwrap();
        assert_eq!(old.wait().unwrap().code(), Some(17));
        result.unwrap();
        assert_eq!(fs::read_to_string(exe).unwrap(), "new");
        assert_eq!(fs::read(backup).unwrap(), fs::read(shell).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_install_lock_releases_inherited_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let first = lock_install_dir(root.path()).unwrap();
        // A clone shares the open-file description just like a concurrent fork's
        // inherited descriptor, without forking a multithreaded test process.
        let inherited = first._file.try_clone().unwrap();
        assert!(lock_install_dir(root.path()).is_err());
        drop(first);

        let next = lock_install_dir(root.path())
            .expect("an inherited descriptor must not extend the finished updater's lease");
        drop(inherited);
        assert!(
            lock_install_dir(root.path()).is_err(),
            "closing the inherited descriptor must not release the next updater's lease"
        );
        drop(next);
        assert!(lock_install_dir(root.path()).is_ok());
    }
}
