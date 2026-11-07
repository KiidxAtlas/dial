use super::*;
use serde_json::json;

#[test]
fn extensions_must_declare_resources_and_own_their_bounded_commits() {
    use crate::tools::{
        ToolOutput,
        integrations::ext::{Tool, ToolContext},
    };
    use std::sync::Arc;
    struct Extension {
        name: &'static str,
        declared: bool,
        serialized: bool,
    }
    impl Tool for Extension {
        fn name(&self) -> &str {
            self.name
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            panic!("planning must not execute tools")
        }
        fn manages_workspace_serialization(&self) -> bool {
            self.serialized
        }
        fn resources(&self, _: &Workspace, _: &Value) -> Option<Access> {
            self.declared.then(|| Access {
                reads: vec![],
                writes: vec!["bounded.txt".into()],
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let mut registry = Registry::new();
    for (name, declared, serialized) in [
        ("unknown", false, true),
        ("unsafe_commit", true, false),
        ("bounded", true, true),
    ] {
        registry
            .try_register(Arc::new(Extension {
                name,
                declared,
                serialized,
            }))
            .unwrap();
    }
    for name in ["unknown", "unsafe_commit"] {
        assert!(resolve(&ws, EditMode::default(), &registry, name, &json!({})).is_none());
    }
    let contract = resolve(&ws, EditMode::default(), &registry, "bounded", &json!({})).unwrap();
    assert!(contract.permits(&[ws.root().join("bounded.txt")], false, false));
    assert!(!contract.permits(&[ws.root().join("other")], false, false));
    assert!(root.path().read_dir().unwrap().next().is_none());
}

#[test]
fn multi_path_reads_conflict_with_each_source_and_searches_with_the_whole_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    for file in ["a", "b", "src/input"] {
        std::fs::write(root.path().join(file), "source").unwrap();
    }
    let ws = Workspace::new(root.path()).unwrap();
    let registry = Registry::new();
    let get = |name, args| resolve(&ws, EditMode::default(), &registry, name, &args).unwrap();
    let read = get("read", json!({"paths":["a","b"]}));
    assert!(read.conflicts(&get("write", json!({"path":"b"}))));
    assert!(!read.conflicts(&get("write", json!({"path":"src/new"}))));
    for name in ["grep", "glob"] {
        let search = get(name, json!({"path":"src"}));
        assert!(
            search.conflicts(&get("write", json!({"path":"src/new"}))),
            "membership includes absent matches"
        );
        assert!(!search.conflicts(&get("write", json!({"path":"a"}))));
    }
    let all = get("grep", json!({"pattern":"not present"}));
    assert!(all.conflicts(&get("write", json!({"path":"new"}))));
}

#[test]
fn native_contracts_refuse_unknown_effects_aliases_and_nonliteral_selectors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "source").unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let registry = Registry::new();
    for (name, args) in [
        ("bash", json!({"command":"cat a"})),
        ("read", json!({"path":"a:1-3"})),
        ("write", json!({"path":"../other"})),
        ("write", json!({"path":"new-parent/file"})),
    ] {
        assert!(
            resolve(&ws, EditMode::default(), &registry, name, &args).is_none(),
            "{name}: {args}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a", root.path().join("alias")).unwrap();
        assert!(
            resolve(
                &ws,
                EditMode::default(),
                &registry,
                "write",
                &json!({"path":"alias"})
            )
            .is_none()
        );
        std::fs::hard_link(root.path().join("a"), root.path().join("hard")).unwrap();
        assert!(
            resolve(
                &ws,
                EditMode::default(),
                &registry,
                "write",
                &json!({"path":"a"})
            )
            .is_none()
        );
    }
}

#[test]
fn read_resources_never_authorize_an_effect_and_unbounded_effects_are_refused() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "source").unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let read = resolve(
        &ws,
        EditMode::default(),
        &Registry::new(),
        "read",
        &json!({"path":"a"}),
    )
    .unwrap();
    let paths = [ws.root().join("a")];
    assert!(read.permits(&paths, false, true));
    assert!(!read.permits(&paths, false, false));
    assert!(!read.permits(&paths, true, true));
}
