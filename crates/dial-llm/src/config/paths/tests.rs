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
