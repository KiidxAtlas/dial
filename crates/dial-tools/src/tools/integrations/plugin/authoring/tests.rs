use super::*;

#[test]
fn scaffold_checks_without_importing_and_refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("hello");
    let python = Path::new("python3");
    create(&destination, Some("hello"), python).unwrap();
    check(&destination, python).unwrap();
    assert!(create(&destination, Some("hello"), python).is_err());
    let entry = destination.join("extension.py");
    fs::write(
        &entry,
        "raise RuntimeError('must not import during check')\n",
    )
    .unwrap();
    check(&destination, python).unwrap();
    fs::write(&entry, "def broken(:\n").unwrap();
    assert!(check(&destination, python).is_err());
}

#[test]
fn generated_python_extension_installs_and_answers_real_host_requests() {
    use crate::tools::integrations::plugin::{
        Host,
        installer::{PluginStore, runtime_spec},
    };
    use serde_json::json;
    use std::sync::Arc;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    create(&source, Some("demo"), Path::new("python3")).unwrap();
    let store = PluginStore::new(temp.path().join("plugins"));
    let installed = store
        .install(&format!("file:{}", source.display()))
        .unwrap();
    let (program, args, env) = runtime_spec(&installed).unwrap().unwrap();
    let host = Arc::new(Host::default());
    host.start_one("demo", &program, &args, &env, 10);
    assert!(host.take_notices().is_empty());
    assert_eq!(host.command_owner("hello").as_deref(), Some("demo"));
    assert_eq!(host.tools().len(), 1);
    assert!(host.tools()[0].needs_approval(&json!({"text":"echo"})));
    let idle = AtomicBool::new(false);
    let first = host
        .request(
            "demo",
            "command",
            json!({"name":"hello","args":"Ada"}),
            &idle,
        )
        .unwrap();
    assert_eq!(first["panel"]["generation"], 1);
    let second = host
        .request(
            "demo",
            "command",
            json!({"name":"hello","args":"Ada"}),
            &idle,
        )
        .unwrap();
    assert_eq!(second["panel"]["generation"], 2);
    let close = host.request("demo", "command.panel", json!({"name":"hello","generation":2,"outcome":{"status":"submitted","button":"save","values":{"name":"Ada"}}}), &idle).unwrap();
    assert_eq!(close["text"], "Hello, Ada");
    assert_eq!(host.take_effects().len(), 1);
    let tool = host.request("demo", "tool.call", json!({"name":"echo","arguments":{"text":"from Python"},"workspace":temp.path().to_string_lossy()}), &idle).unwrap();
    assert_eq!(tool, json!({"text":"from Python","is_error":false}));
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("dial-plugin.json")).unwrap()).unwrap();
    manifest["version"] = json!("0.2.0");
    fs::write(
        source.join("dial-plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(store.update(Some("demo")).unwrap()[0].changed);
    // A live host keeps the admitted contract, never silently executing the updated package.
    assert!(
        host.request(
            "demo",
            "command",
            json!({"name":"hello","args":"Ada"}),
            &idle
        )
        .unwrap_err()
        .to_string()
        .contains("installation changed")
    );
    store.set_enabled("demo", false).unwrap();
    assert!(store.enabled_runtimes().unwrap().is_empty());
    store.remove("demo").unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn shipped_examples_initialize_through_the_installed_host() {
    use crate::tools::integrations::plugin::{
        Host,
        installer::{PluginStore, runtime_spec},
    };
    let temp = tempfile::tempdir().unwrap();
    let store = PluginStore::new(temp.path().join("plugins"));
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/examples");
    for (name, command, capability) in [
        ("build-dashboard", "build-dashboard", "views"),
        ("pr-review", "pr-review", "host.models"),
        ("workflow-pack", "personal-review", "host.workflows"),
    ] {
        let source = examples.join(name);
        let installed = store
            .install(&format!("file:{}", source.display()))
            .unwrap();
        let (program, args, env) = runtime_spec(&installed).unwrap().unwrap();
        let host = Host::default();
        host.start_one(name, &program, &args, &env, 10);
        assert!(
            host.take_notices().is_empty(),
            "{name} initialization produced notices"
        );
        assert_eq!(host.command_owner(command).as_deref(), Some(name));
        assert!(host.registration(name).unwrap().1.has(capability));
        let contributions = host.contributions(name).unwrap();
        super::super::contributions::validate(&contributions).unwrap();
        if name == "workflow-pack" {
            assert_eq!(contributions.agents[0].id, "reviewer");
            assert_eq!(contributions.workflows[0].steps.len(), 3);
            assert_eq!(contributions.context_sources[0].id, "preferences");
        } else {
            assert_eq!(host.views().len(), 1);
            assert_eq!(host.views()[0].revision, 1);
        }
        host.close();
        store.remove(name).unwrap();
    }
}
