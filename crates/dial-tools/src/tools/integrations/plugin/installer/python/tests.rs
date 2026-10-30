use super::super::{PluginStore, runtime_spec, test_support::write_plugin};
use super::*;
use serde_json::json;

fn environment_count(store: &PluginStore) -> usize {
    fs::read_dir(store.root.join(".python-envs"))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

fn assert_no_staging(store: &PluginStore) {
    assert!(!fs::read_dir(&store.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")
    }));
}

#[test]
fn invalid_python_declarations_never_install() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = PluginStore::new(temp.path().join("store"));
    let cases = [
        json!({"entry":"../outside.py"}),
        json!({"entry":"/tmp/extension.py"}),
        json!({"entry":"missing.py"}),
        json!({"entry":"extension.py", "unknown":true}),
        json!({"entry":"extension.py", "dependencies":[1]}),
        json!({"entry":"extension.py", "dependencies":["--target=/tmp"]}),
        json!({"entry":"extension.py", "interpreter":null}),
        json!({"entry":"extension.txt"}),
        json!({"entry":"extension.py", "dependencies":["   "]}),
        json!({"entry":"extension.py", "dependencies":["  --target=/tmp"]}),
    ];
    for python in cases {
        write_plugin(
            &source,
            &json!({"name":"sample", "python":python}).to_string(),
        );
        fs::write(source.join("extension.py"), "pass\n").unwrap();
        fs::write(source.join("extension.txt"), "pass\n").unwrap();
        assert!(
            store
                .install(&format!("file:{}", source.display()))
                .is_err(),
            "{python}"
        );
        assert!(store.list().unwrap().is_empty());
    }
    for extra in [
        json!({"program":"extension.py"}),
        json!({"settings":[]}),
        json!({"env":{"DIAL_EXTENSION_ROOT":"/tmp"}}),
    ] {
        let mut manifest = json!({"name":"sample", "python":{"entry":"extension.py"}});
        manifest
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        write_plugin(&source, &manifest.to_string());
        assert!(
            store
                .install(&format!("file:{}", source.display()))
                .is_err(),
            "{extra}"
        );
    }
    write_plugin(
        &source,
        &json!({"name":"sample", "python":{"entry":"extension.py"}}).to_string(),
    );
    fs::create_dir(source.join(RUNTIME)).unwrap();
    assert!(
        store
            .install(&format!("file:{}", source.display()))
            .is_err()
    );
}

#[test]
fn failed_python_setup_cleans_staging_and_preserves_previous_install() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = PluginStore::new(temp.path().join("store"));
    write_plugin(&source, r#"{"name":"sample","version":"1"}"#);
    let original = store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    write_plugin(
        &source,
        r#"{"name":"sample","version":"2","python":{"entry":"extension.py","interpreter":"dial-test-nonexistent-python-interpreter"}}"#,
    );
    fs::write(source.join("extension.py"), "pass\n").unwrap();
    assert!(
        store.check_updates().unwrap()[0].changed,
        "checking updates must not execute the interpreter"
    );
    assert!(store.update(Some("sample")).is_err());
    assert_eq!(store.get("sample").unwrap().integrity, original.integrity);
    assert_eq!(environment_count(&store), 0);
    assert_no_staging(&store);
    store.remove("sample").unwrap();
    assert!(
        store
            .install(&format!("file:{}", source.display()))
            .is_err()
    );
    assert!(store.list().unwrap().is_empty());
    assert_eq!(environment_count(&store), 0);
    assert_no_staging(&store);
}

#[test]
fn python_environment_is_stable_and_state_survives_update() {
    // This default-suite check uses only the already-installed Python stdlib, never pip/network.
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let store = PluginStore::new(temp.path().join("store"));
    write_plugin(
        &source,
        r#"{"name":"sample","python":{"entry":"extension.py"},"settings":{"greeting":"hello"}}"#,
    );
    fs::write(source.join("extension.py"), "pass\n").unwrap();
    // Local developer virtualenvs can contain symlinks and must not be copied.
    fs::create_dir(source.join(".venv")).unwrap();
    fs::write(source.join(".venv/ignored"), "local environment").unwrap();
    let installed = store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    let (program, args, env) = runtime_spec(&installed).unwrap().unwrap();
    assert_eq!(env["DIAL_EXTENSION_NAME"], "sample");
    assert!(program.starts_with(fs::canonicalize(store.root.join(".python-envs")).unwrap()));
    assert_eq!(environment_count(&store), 1);
    assert!(!installed.path.join(".venv").exists());
    assert_eq!(args[0], "-I");
    assert_eq!(
        serde_json::from_str::<Value>(&env["DIAL_EXTENSION_SETTINGS"]).unwrap(),
        json!({"greeting":"hello"})
    );
    let mut command = Command::new(&program);
    command.args(["-I", "-c", "import sys; assert sys.prefix != sys.base_prefix; import pathlib; assert pathlib.Path(sys.prefix).is_dir()"]);
    run(command, "checking stable venv").unwrap();
    let state = PathBuf::from(&env["DIAL_EXTENSION_STATE_DIR"]);
    fs::write(state.join("persistent"), "retained").unwrap();
    fs::write(source.join("extension.py"), "# update\npass\n").unwrap();
    let lease = super::super::super::lease::shared(&store.root, "sample").unwrap();
    assert!(
        store.update(Some("sample")).is_err(),
        "active request prevents publication"
    );
    assert!(program.is_file(), "busy update retains the old environment");
    assert_eq!(
        environment_count(&store),
        1,
        "busy update drops prepared environment"
    );
    drop(lease);
    assert!(store.update(Some("sample")).unwrap()[0].changed);
    let updated = store.get("sample").unwrap();
    let (_, _, env) = runtime_spec(&updated).unwrap().unwrap();
    assert!(
        !program.exists(),
        "successful update removes old environment after commit"
    );
    assert_eq!(environment_count(&store), 1);
    assert_eq!(PathBuf::from(&env["DIAL_EXTENSION_STATE_DIR"]), state);
    assert_eq!(
        fs::read_to_string(state.join("persistent")).unwrap(),
        "retained"
    );
    fs::remove_file(updated.path.join(RUNTIME).join("ready")).unwrap();
    assert!(
        runtime_spec(&updated)
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
    assert!(
        !updated.path.join(RUNTIME).join("ready").exists(),
        "load must not repair or install"
    );
    assert!(
        store.update(Some("sample")).unwrap()[0].changed,
        "explicit update repairs incomplete environment"
    );
    assert!(
        runtime_spec(&store.get("sample").unwrap())
            .unwrap()
            .is_some()
    );
    let pointer = updated.path.join(RUNTIME).join("environment");
    let original = fs::read_to_string(&pointer).unwrap();
    for invalid in [
        "../outside",
        "/tmp/env-other",
        "env-../../outside",
        "env-a/b",
    ] {
        fs::write(&pointer, invalid).unwrap();
        assert!(runtime_spec(&updated).is_err(), "{invalid}");
        assert!(
            store.remove("sample").is_err(),
            "unsafe pointer must never be followed for cleanup"
        );
        assert!(updated.path.exists());
    }
    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "must survive").unwrap();
        let link = store.root.join(".python-envs/env-escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        fs::write(&pointer, "env-escape").unwrap();
        assert!(runtime_spec(&updated).is_err());
        assert!(store.remove("sample").is_err());
        assert!(outside.join("sentinel").is_file());
        fs::remove_file(link).unwrap();
    }
    fs::write(&pointer, original).unwrap();
    store.remove("sample").unwrap();
    assert_eq!(environment_count(&store), 0);
    assert!(
        state.join("persistent").is_file(),
        "remove retains persistent state"
    );
}

#[cfg(unix)]
#[test]
fn dependency_install_failure_is_bounded_to_staging() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let interpreter = temp.path().join("fake-python");
    // A deterministic interpreter fixture creates only a failing local pip runner.
    fs::write(
        &interpreter,
        r#"#!/bin/sh
set -eu
test "$1" = -I
if [ "$2" = -c ]; then exit 0; fi
test "$2" = -m
test "$3" = venv
mkdir -p "$4/bin"
printf '#!/bin/sh\necho dependency-resolution-failed >&2\nexit 42\n' > "$4/bin/python"
chmod +x "$4/bin/python"
"#,
    )
    .unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    write_plugin(&source, &json!({"name":"sample", "python":{"entry":"extension.py", "interpreter":interpreter, "dependencies":["example==1"]}}).to_string());
    fs::write(source.join("extension.py"), "pass\n").unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    let error = store
        .install(&format!("file:{}", source.display()))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("dependency-resolution-failed"),
        "{error:#}"
    );
    assert!(store.list().unwrap().is_empty());
    assert_eq!(environment_count(&store), 0);
    assert_no_staging(&store);
}

#[cfg(unix)]
#[test]
fn generated_console_shebang_remains_valid_after_install_and_update() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let interpreter = temp.path().join("fixture-python");
    // Mimic an installer-generated console executable during environment preparation.
    // No package indexes or global package installation are involved.
    fs::write(&interpreter, r##"#!/bin/sh
set -eu
if [ "$2" = -c ]; then exec python3 "$@"; fi
python3 "$@"
"$4/bin/python" -I -c 'import pathlib,sys; p=pathlib.Path(sys.prefix)/"bin/dial-fixture"; p.write_text("#!"+sys.executable+"\nimport sys\nprint(sys.prefix)\n"); p.chmod(0o700)'
"##).unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    write_plugin(
        &source,
        &json!({"name":"console", "python":{"entry":"extension.py", "interpreter":interpreter}})
            .to_string(),
    );
    fs::write(source.join("extension.py"), "pass\n").unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    let mut previous: Option<PathBuf> = None;
    for revision in 0..2 {
        if revision > 0 {
            fs::write(source.join("extension.py"), "# updated\npass\n").unwrap();
            store.update(Some("console")).unwrap();
        }
        let installed = store.get("console").unwrap();
        let (program, _, _) = runtime_spec(&installed).unwrap().unwrap();
        let console = program.parent().unwrap().join("dial-fixture");
        let text = fs::read_to_string(&console).unwrap();
        assert_eq!(
            text.lines().next().unwrap(),
            format!("#!{}", program.display())
        );
        run(
            Command::new(&console),
            "running generated console executable",
        )
        .unwrap();
        if let Some(previous) = previous {
            assert!(!previous.exists());
        }
        previous = Some(console);
    }
    store.remove("console").unwrap();
    assert_eq!(environment_count(&store), 0);
}

#[test]
fn lockfiles_are_confined_and_runtime_selection_is_explicit() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("extension.py"), "pass\n").unwrap();
    fs::write(
        temp.path().join("requirements.lock"),
        "example==1.0 \\\n    --hash=sha256:0123456789abcdef\n",
    )
    .unwrap();
    for runtime in ["request", "worker"] {
        let manifest = json!({"python":{"entry":"extension.py","runtime":runtime,"lockfile":"requirements.lock"}});
        assert!(declaration(manifest.as_object().unwrap(), temp.path()).is_ok());
    }
    for python in [
        json!({"entry":"extension.py","runtime":"unknown"}),
        json!({"entry":"extension.py","lockfile":"../requirements.lock"}),
        json!({"entry":"extension.py","lockfile":"requirements.lock","dependencies":["example"]}),
    ] {
        assert!(declaration(json!({"python":python}).as_object().unwrap(), temp.path()).is_err());
    }
    fs::write(
        temp.path().join("requirements.lock"),
        "-r ../../outside.txt\n",
    )
    .unwrap();
    assert!(
        declaration(
            json!({"python":{"entry":"extension.py","lockfile":"requirements.lock"}})
                .as_object()
                .unwrap(),
            temp.path()
        )
        .is_err()
    );
}
