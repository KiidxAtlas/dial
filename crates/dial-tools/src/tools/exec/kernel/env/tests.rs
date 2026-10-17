use super::*;

fn venv(root: &Path, name: &str, config: &str) -> PathBuf {
    let prefix = root.join(name);
    std::fs::create_dir_all(prefix.join("lib/python3.12/site-packages")).unwrap();
    std::fs::write(prefix.join("pyvenv.cfg"), config).unwrap();
    prefix
}

#[test]
fn pyvenv_cfg_parses_home_version_and_system_site() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = venv(
        dir.path(),
        ".venv",
        &format!(
            "home = {}\n# note\ninclude-system-site-packages = false\nversion = 3.12.4\n",
            dir.path().join("base/bin").display()
        ),
    );
    let found = inspect_layout(&prefix, "workspace .venv".into(), false).unwrap();
    assert_eq!(found.kind, EnvKind::Venv);
    assert_eq!(found.prefix, prefix);
    assert!(!found.include_system_site);
    assert_eq!(
        found.launchers,
        vec![
            prefix.join("bin/python"),
            dir.path().join("base/bin/python3.12"),
            dir.path().join("base/bin/python3"),
            dir.path().join("base/bin/python"),
        ]
    );
    assert_eq!(found.source, "workspace .venv");
}

#[test]
fn auto_detection_order_and_opt_outs() {
    let dir = tempfile::tempdir().unwrap();
    let active = venv(dir.path(), "active", "home = /usr/bin\n");
    venv(dir.path(), ".venv", "home = /usr/bin\n");
    let conda = dir.path().join("conda");
    std::fs::create_dir_all(conda.join("conda-meta")).unwrap();
    let both = |name: &str| match name {
        "VIRTUAL_ENV" => Some(active.to_string_lossy().into_owned()),
        "CONDA_PREFIX" => Some(conda.to_string_lossy().into_owned()),
        _ => None,
    };
    let found = detect(dir.path(), "auto", "python3", &both)
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.prefix.clone(), found.source.as_str()),
        (active.clone(), "VIRTUAL_ENV")
    );
    let conda_only =
        |name: &str| (name == "CONDA_PREFIX").then(|| conda.to_string_lossy().into_owned());
    let found = detect(dir.path(), "auto", "python3", &conda_only)
        .unwrap()
        .unwrap();
    assert_eq!(found.kind, EnvKind::Conda);
    // A stale VIRTUAL_ENV is skipped rather than failing the kernel.
    let stale = |name: &str| (name == "VIRTUAL_ENV").then(|| "/nonexistent/venv".to_owned());
    let found = detect(dir.path(), "auto", "python3", &stale)
        .unwrap()
        .unwrap();
    assert_eq!(found.source, "workspace .venv");
    assert!(
        detect(dir.path(), "off", "python3", &both)
            .unwrap()
            .is_none()
    );
    // An explicitly configured interpreter bypasses discovery.
    assert!(
        detect(dir.path(), "auto", "/opt/python3.11", &both)
            .unwrap()
            .is_none()
    );
}

#[test]
fn explicit_setting_must_name_an_environment() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = venv(dir.path(), "envs/data", "home = /usr/bin\n");
    let found = detect(dir.path(), "envs/data", "/opt/python", &|_| None)
        .unwrap()
        .unwrap();
    assert_eq!(found.prefix, prefix);
    assert_eq!(found.source, "kernel.venv");
    assert!(
        detect(dir.path(), "missing", "python3", &|_| None)
            .unwrap_err()
            .contains("neither")
    );
}

#[test]
fn site_packages_must_match_the_running_interpreter() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = venv(dir.path(), ".venv", "home = /usr/bin\n");
    assert_eq!(
        site_packages_layout(&prefix, (3, 12), false).unwrap(),
        prefix.join("lib/python3.12/site-packages")
    );
    let error = site_packages_layout(&prefix, (3, 11), false).unwrap_err();
    assert!(
        error.contains("python3.12") && error.contains("3.11"),
        "{error}"
    );
}

#[test]
fn windows_venv_uses_base_interpreter_and_unversioned_site() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("Python312");
    let executable = base.join("python.exe");
    let prefix = venv(
        dir.path(),
        ".venv",
        &format!(
            "home = {}\nexecutable = {}\nversion = 3.12.4\ninclude-system-site-packages = true\n",
            base.display(),
            executable.display()
        ),
    );
    std::fs::create_dir_all(prefix.join("Scripts")).unwrap();
    std::fs::write(prefix.join("Scripts/python.exe"), "redirector").unwrap();
    std::fs::create_dir_all(prefix.join("Lib/site-packages")).unwrap();
    let found = inspect_layout(&prefix, "fixture".into(), true).unwrap();
    assert_eq!(found.launchers, vec![executable.clone(), executable]);
    assert!(found.include_system_site);
    assert_eq!(
        site_packages_layout(&prefix, (3, 12), true).unwrap(),
        prefix.join("Lib/site-packages")
    );
    std::fs::remove_dir(prefix.join("Lib/site-packages")).unwrap();
    assert!(site_packages_layout(&prefix, (3, 12), true).is_err());
}

#[test]
fn windows_venv_home_fallback_and_relative_base_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("Python312");
    let prefix = venv(dir.path(), ".venv", &format!("home = {}\n", base.display()));
    let found = inspect_layout(&prefix, "fixture".into(), true).unwrap();
    assert_eq!(found.launchers, vec![base.join("python.exe")]);
    std::fs::write(
        prefix.join("pyvenv.cfg"),
        "home = relative\nexecutable = python.exe\n",
    )
    .unwrap();
    assert!(
        inspect_layout(&prefix, "fixture".into(), true)
            .unwrap()
            .launchers
            .is_empty()
    );
}

#[test]
fn conda_interpreter_layouts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("conda-meta")).unwrap();
    for windows in [false, true] {
        let found = inspect_layout(dir.path(), "fixture".into(), windows).unwrap();
        assert_eq!(found.kind, EnvKind::Conda);
        assert_eq!(
            found.launchers,
            if windows {
                vec![dir.path().join("python.exe")]
            } else {
                vec![
                    dir.path().join("bin/python3"),
                    dir.path().join("bin/python"),
                ]
            }
        );
    }
}
