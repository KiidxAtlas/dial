use super::test_support::{commit_plugin, git, write_plugin};
use super::*;
use serde_json::json;

#[test]
fn git_install_records_commit_sha_and_refuses_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let sha = commit_plugin(&repo, r#"{"name":"gitplug","version":"1.0.0"}"#);

    let store = PluginStore::new(temp.path().join("store-a"));
    let installed = store
        .install(&format!("git+file://{}", repo.display()))
        .unwrap();
    assert_eq!(installed.name, "gitplug");
    assert_eq!(installed.version.as_deref(), Some("1.0.0"));
    assert_eq!(installed.integrity, sha);
    assert!(installed.enabled);
    assert!(installed.path.join("dial-plugin.json").is_file());
    assert!(!installed.path.join(".git").exists());

    let err = store
        .install(&format!("git+{}", repo.display()))
        .unwrap_err();
    assert!(format!("{err:#}").contains("already installed"));

    let other = PluginStore::new(temp.path().join("store-b"));
    let plain = other.install(&format!("git+{}", repo.display())).unwrap();
    assert_eq!(plain.integrity, sha);
}

#[test]
fn git_update_reports_changed_after_new_commit() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    commit_plugin(&repo, r#"{"name":"gitplug","version":"1.0.0"}"#);
    let store = PluginStore::new(temp.path().join("store"));
    let first = store.install(&format!("git+{}", repo.display())).unwrap();
    store.set_enabled("gitplug", false).unwrap();

    let unchanged = store.update(Some("gitplug")).unwrap();
    assert_eq!(unchanged.len(), 1);
    assert!(!unchanged[0].changed);

    fs::write(
        repo.join("dial-plugin.json"),
        r#"{"name":"gitplug","version":"2.0.0"}"#,
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "v2"],
    );
    let new_sha = git(&repo, &["rev-parse", "HEAD"]);

    let updates = store.check_updates().unwrap();
    assert!(updates[0].changed);
    assert_eq!(
        store.get("gitplug").unwrap().integrity,
        first.integrity,
        "check_updates must not install"
    );

    let updates = store.update(Some("gitplug")).unwrap();
    assert!(updates[0].changed);
    let plugin = store.get("gitplug").unwrap();
    assert_eq!(plugin.integrity, new_sha);
    assert_eq!(plugin.version.as_deref(), Some("2.0.0"));
    assert!(!plugin.enabled, "update must keep the enabled flag");

    let again = store.update(None).unwrap();
    assert!(!again[0].changed);
}

#[test]
fn file_install_records_tree_digest() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    write_plugin(&source, r#"{"name":"filed","version":"0.1.0"}"#);
    fs::create_dir_all(source.join("lib")).unwrap();
    fs::write(source.join("lib/data.txt"), "hello").unwrap();

    let store = PluginStore::new(temp.path().join("a"));
    let installed = store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    assert_eq!(installed.name, "filed");
    assert!(installed.integrity.starts_with("sha256:"));
    assert_eq!(installed.integrity.len(), "sha256:".len() + 64);
    assert_eq!(
        fs::read_to_string(installed.path.join("lib/data.txt")).unwrap(),
        "hello"
    );

    let other = PluginStore::new(temp.path().join("b"));
    let again = other
        .install(&format!("file:{}", source.display()))
        .unwrap();
    assert_eq!(
        again.integrity, installed.integrity,
        "digest is deterministic"
    );
}

#[test]
fn invalid_names_are_rejected_before_path_joins() {
    assert!(validate_name("../x").is_err());
    assert!(validate_name("a/b").is_err());
    assert!(validate_name("").is_err());
    assert!(validate_name(".hidden").is_err());
    assert!(validate_name(&"a".repeat(129)).is_err());
    assert!(validate_name("ok-name_1.2").is_ok());

    let temp = tempfile::tempdir().unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    assert!(store.get("../x").is_err());
    assert!(store.remove("../x").is_err());
    assert!(store.set_enabled("../x", true).is_err());

    let source = temp.path().join("evil");
    write_plugin(&source, r#"{"name":"../escape"}"#);
    assert!(
        store
            .install(&format!("file:{}", source.display()))
            .is_err()
    );
    assert!(!temp.path().join("escape").exists());
}

#[test]
fn enable_disable_round_trip_and_runtime_spec() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    write_plugin(
        &source,
        r#"{"name":"rt","program":"bin/run.sh","args":["--x"],"env":{"K":"V"}}"#,
    );
    fs::create_dir_all(source.join("bin")).unwrap();
    fs::write(source.join("bin/run.sh"), "#!/bin/sh\n").unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    store
        .install(&format!("file:{}", source.display()))
        .unwrap();

    store.set_enabled("rt", false).unwrap();
    assert!(!store.list().unwrap()[0].enabled);
    assert!(store.enabled_runtimes().unwrap().is_empty());
    assert!(runtime_spec(&store.get("rt").unwrap()).unwrap().is_none());

    store.set_enabled("rt", true).unwrap();
    assert!(store.get("rt").unwrap().enabled);
    let runtimes = store.enabled_runtimes().unwrap();
    assert_eq!(runtimes.len(), 1);
    let (plugin, program, args, env) = runtimes[0].as_ref().unwrap();
    assert_eq!(plugin.name, "rt");
    assert!(program.ends_with("bin/run.sh"));
    assert_eq!(args, &vec!["--x".to_string()]);
    assert_eq!(env.get("K").map(String::as_str), Some("V"));
}

#[test]
fn runtime_program_must_stay_inside_plugin() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    write_plugin(&source, r#"{"name":"esc","program":"../outside"}"#);
    fs::write(temp.path().join("outside"), "x").unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    // A failed fresh install must not leave an enabled record behind.
    let error = store
        .install(&format!("file:{}", source.display()))
        .unwrap_err();
    assert!(format!("{error:#}").contains("esc"), "{error:#}");
    assert!(store.get("esc").is_err());
    assert!(store.list().unwrap().is_empty());
    assert!(!store.root.join("esc").exists());
}

#[test]
fn broken_plugin_does_not_hide_healthy_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let store = PluginStore::new(temp.path().join("store"));
    for name in ["alpha", "beta", "gamma"] {
        let source = temp.path().join(name);
        write_plugin(
            &source,
            &format!(r#"{{"name":"{name}","program":"bin/run.sh"}}"#),
        );
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::write(source.join("bin/run.sh"), "#!/bin/sh\n").unwrap();
        store
            .install(&format!("file:{}", source.display()))
            .unwrap();
    }
    // The executable disappears after installation, so only beta is broken at startup.
    fs::remove_file(store.get("beta").unwrap().path.join("bin/run.sh")).unwrap();
    let outcomes = store.enabled_runtimes().unwrap();
    assert_eq!(outcomes.len(), 3);
    let healthy: Vec<&str> = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .map(|(plugin, ..)| plugin.name.as_str())
        .collect();
    assert_eq!(healthy, vec!["alpha", "gamma"]);
    let failure = outcomes
        .iter()
        .find_map(|outcome| outcome.as_ref().err())
        .unwrap();
    assert!(
        format!("{failure:#}").contains("plugin beta"),
        "{failure:#}"
    );
}

#[test]
fn remove_deletes_plugin() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    write_plugin(&source, r#"{"name":"gone"}"#);
    let store = PluginStore::new(temp.path().join("store"));
    store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    store.remove("gone").unwrap();
    assert!(store.list().unwrap().is_empty());
    assert!(store.get("gone").is_err());
}

#[test]
fn compatibility_and_explicit_purge_boundaries() {
    for api in [json!(1), json!({"min":1,"max":1}), json!({"min":0,"max":2})] {
        assert!(validate_compatibility(json!({"apiVersion":api}).as_object().unwrap()).is_ok());
    }
    for api in [
        json!(2),
        json!("1"),
        json!({"min":2,"max":3}),
        json!({"min":1}),
        json!({"min":1,"max":1,"unknown":true}),
    ] {
        assert!(validate_compatibility(json!({"apiVersion":api}).as_object().unwrap()).is_err());
    }
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    write_plugin(&source, r#"{"name":"gone"}"#);
    let store = PluginStore::new(temp.path().join("store"));
    store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    for name in ["gone", "keep"] {
        fs::create_dir_all(store.root.join(".state").join(name)).unwrap();
    }
    fs::create_dir_all(store.root.join(".settings")).unwrap();
    fs::write(store.root.join(".settings/gone.json"), "{}").unwrap();
    store.remove("gone").unwrap();
    assert!(store.root.join(".state/gone").exists());
    store.purge_state("gone").unwrap();
    assert!(!store.root.join(".state/gone").exists());
    assert!(store.root.join(".state/keep").exists());
    assert!(!store.root.join(".settings/gone.json").exists());
    assert!(store.purge_state("../keep").is_err());
}
