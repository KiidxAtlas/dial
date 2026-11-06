use super::*;

#[test]
fn state_is_per_project_and_private_workspaces_are_rejected() {
    let home = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (la, lb) = (
        Layout::with_user_dir(a.path(), home.path().into()).unwrap(),
        Layout::with_user_dir(b.path(), home.path().into()).unwrap(),
    );
    assert_ne!(la.state_dir, lb.state_dir);
    la.ensure_project_outside_storage().unwrap();
    let inside = Layout::with_user_dir(a.path(), a.path().join(".dialhome")).unwrap();
    inside.ensure_project_outside_storage().unwrap();
    let private_project = a.path().join(".dialhome/private");
    fs::create_dir_all(&private_project).unwrap();
    let private = Layout::with_user_dir(&private_project, a.path().join(".dialhome")).unwrap();
    assert!(private.ensure_project_outside_storage().is_err());
}

#[test]
fn home_directory_can_be_the_project_without_moving_state() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
    layout.ensure_project_outside_storage().unwrap();
    assert_eq!(layout.project_root, fs::canonicalize(home.path()).unwrap());
    assert_eq!(layout.user_dir, layout.project_root.join(".dial"));
    assert!(layout.state_dir.starts_with(&layout.user_dir));
    assert_eq!(
        layout.config_files()[0],
        layout.user_dir.join("config.toml")
    );
}

#[test]
fn home_resolution_preserves_home_and_uses_profile_only_on_windows() {
    let home = PathBuf::from("home");
    let profile = PathBuf::from("profile");
    for windows in [false, true] {
        assert_eq!(
            home_dir_from(Some("home".into()), windows, || panic!("HOME wins")),
            Some(home.clone())
        );
        for missing in [None, Some("".into())] {
            assert_eq!(
                home_dir_from(missing, windows, || Some("profile".into())),
                windows.then(|| profile.clone())
            );
        }
    }
    assert_eq!(home_dir_from(None, true, || None), None);
    assert_eq!(home_dir_from(None, true, || Some("".into())), None);
}

#[test]
fn dial_home_precedence_and_missing_home_are_explicit() {
    assert_eq!(
        user_dir_from(Some("custom".into()), || panic!("DIAL_HOME wins")).unwrap(),
        PathBuf::from("custom")
    );
    for missing in [None, Some("".into())] {
        assert_eq!(
            user_dir_from(missing.clone(), || Some(PathBuf::from("home"))).unwrap(),
            PathBuf::from("home").join(".dial")
        );
        assert!(user_dir_from(missing, || None).is_err());
    }
}

#[test]
fn subscription_auth_override_is_explicit_absolute_and_separate_from_state() {
    let isolated = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    assert_eq!(
        subscription_auth_dir_from(isolated.path(), None).unwrap(),
        isolated.path()
    );
    assert_eq!(
        subscription_auth_dir_from(isolated.path(), Some(shared.path().as_os_str().to_owned()))
            .unwrap(),
        fs::canonicalize(shared.path()).unwrap()
    );
    for invalid in [
        PathBuf::new(),
        PathBuf::from("relative"),
        shared.path().join("missing"),
    ] {
        assert!(
            subscription_auth_dir_from(isolated.path(), Some(invalid.into_os_string())).is_err()
        );
    }
    let file = shared.path().join("not-a-directory");
    fs::write(&file, b"not credentials").unwrap();
    assert!(subscription_auth_dir_from(isolated.path(), Some(file.into_os_string())).is_err());
    assert_eq!(fs::read_dir(isolated.path()).unwrap().count(), 0);
}

#[test]
fn grouped_records_copy_without_removing_legacy_and_new_wins() {
    for group in ["auth", "state", "cache"] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let legacy = layout.user_dir.join("record.json");
        let target = layout.user_dir.join(group).join("record.json");
        assert_eq!(grouped_file(&layout.user_dir, group, "record.json"), target);
        fs::write(&legacy, b"old").unwrap();
        assert_eq!(grouped_file(&layout.user_dir, group, "record.json"), target);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read(&legacy).unwrap(), b"old");
        let migrated = fs::File::open(&target).unwrap();
        assert!(crate::platform::fs::is_owner_only(&migrated).unwrap());
        fs::write(&target, b"new").unwrap();
        assert_eq!(grouped_file(&layout.user_dir, group, "record.json"), target);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        let lock = super::super::settings_store::locked(&target).unwrap();
        assert!(super::super::settings_store::try_locked(&legacy).is_err());
        drop(lock);
    }
}

#[test]
fn grouped_record_failure_and_busy_lock_fall_back_to_legacy() {
    for blocked_directory in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join("record.json");
        fs::write(&legacy, b"original").unwrap();
        let lock = if blocked_directory {
            fs::write(home.path().join("auth"), b"not a directory").unwrap();
            None
        } else {
            Some(super::super::settings_store::locked(&legacy).unwrap())
        };
        assert_eq!(auth_file(home.path(), "record.json"), legacy);
        assert_eq!(fs::read(&legacy).unwrap(), b"original");
        assert!(!home.path().join("auth/record.json").exists());
        drop(lock);
    }
}
