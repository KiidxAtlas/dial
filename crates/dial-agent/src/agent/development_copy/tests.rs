use super::*;
#[cfg(unix)]
#[test]
fn copied_virtual_environment_executes_with_its_private_prefix() {
    let source = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    let setup = std::process::Command::new("python3")
        .args(["-m", "venv", "--without-pip"])
        .arg(source.path().join(".venv"))
        .output()
        .unwrap();
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
    dependencies(source.path(), dest.path(), 128 * 1024 * 1024).unwrap();
    let run = std::process::Command::new(dest.path().join(".venv/bin/python"))
        .args([
            "-c",
            "import sys,pathlib; print(pathlib.Path(sys.prefix).resolve())",
        ])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim(),
        dest.path()
            .join(".venv")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn executable_links_stay_private_and_environment_launchers_are_relocated() {
    let source = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    fs::create_dir_all(source.path().join("node_modules/.bin")).unwrap();
    fs::create_dir_all(source.path().join("node_modules/pkg/bin")).unwrap();
    fs::write(source.path().join("node_modules/pkg/bin/cli"), "original").unwrap();
    std::os::unix::fs::symlink(
        "../pkg/bin/cli",
        source.path().join("node_modules/.bin/cli"),
    )
    .unwrap();
    fs::create_dir_all(source.path().join(".venv/bin")).unwrap();
    fs::write(
        source.path().join(".venv/bin/pytest"),
        format!("#!{}/.venv/bin/python\n", source.path().display()),
    )
    .unwrap();
    fs::create_dir_all(source.path().join("packages/app/node_modules/pkg")).unwrap();
    fs::write(
        source.path().join("packages/app/node_modules/pkg/index.js"),
        "nested",
    )
    .unwrap();
    dependencies(source.path(), dest.path(), 4096).unwrap();
    assert!(
        fs::symlink_metadata(dest.path().join("node_modules/.bin/cli"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::write(dest.path().join("node_modules/.bin/cli"), "private edit").unwrap();
    assert_eq!(
        fs::read_to_string(source.path().join("node_modules/pkg/bin/cli")).unwrap(),
        "original"
    );
    let launcher = fs::read_to_string(dest.path().join(".venv/bin/pytest")).unwrap();
    assert!(launcher.starts_with(&format!(
        "#!{}/.venv/bin/python",
        dest.path().canonicalize().unwrap().display()
    )));
    assert!(
        dest.path()
            .join("packages/app/node_modules/pkg/index.js")
            .exists()
    );
}

#[test]
fn dependency_edits_do_not_touch_parent_and_secrets_are_omitted() {
    let source = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    fs::create_dir(source.path().join("node_modules")).unwrap();
    fs::write(source.path().join("node_modules/pkg.js"), "original").unwrap();
    fs::write(source.path().join("node_modules/.env"), "secret").unwrap();
    dependencies(source.path(), dest.path(), 1024).unwrap();
    fs::write(dest.path().join("node_modules/pkg.js"), "changed").unwrap();
    assert_eq!(
        fs::read_to_string(source.path().join("node_modules/pkg.js")).unwrap(),
        "original"
    );
    assert!(!dest.path().join("node_modules/.env").exists());
    assert!(dependencies(source.path(), tempfile::tempdir().unwrap().path(), 1).is_err());
}
