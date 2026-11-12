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

#[cfg(unix)]
#[test]
fn dotfile_sources_and_scoped_packages_resolve_but_metadata_and_handles_stay_serial() {
    let root = tempfile::tempdir().unwrap();
    for dir in [".github", ".git", ".dial", "node_modules/@scope"] {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    for file in [
        ".github/ci.yml",
        ".git/config",
        ".dial/state",
        "node_modules/@scope/index.js",
        "icon@2x.png",
    ] {
        std::fs::write(root.path().join(file), "source").unwrap();
    }
    let ws = Workspace::new(root.path()).unwrap();
    let registry = Registry::new();
    let get = |name, args: Value| resolve(&ws, EditMode::default(), &registry, name, &args);
    for path in [
        ".github/ci.yml",
        "node_modules/@scope/index.js",
        ".env.example",
    ] {
        assert!(get("write", json!({"path":path})).is_some(), "{path}");
    }
    assert!(get("read", json!({"path":"node_modules/@scope/index.js"})).is_some());
    for path in [
        ".git/config",
        ".dial/state",
        "@f7",
        "@f12#AB",
        "user@host/file",
        "icon@2x.png",
        "https://example.com/a",
    ] {
        assert!(get("write", json!({"path":path})).is_none(), "{path}");
    }
}

#[test]
fn effect_free_external_reads_have_an_empty_footprint() {
    use crate::tools::{
        ToolOutput,
        integrations::ext::{Concurrency, Tool, ToolContext},
    };
    use std::sync::Arc;
    struct External {
        name: &'static str,
        concurrency: Concurrency,
        modifies: bool,
    }
    impl Tool for External {
        fn name(&self) -> &str {
            self.name
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn may_modify_workspace(&self, _: &Value) -> bool {
            self.modifies
        }
        fn concurrency(&self, _: &Value) -> Concurrency {
            self.concurrency
        }
        fn resources(&self, _: &Workspace, _: &Value) -> Option<Access> {
            Some(Access::default())
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            panic!("planning must not execute tools")
        }
    }
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let mut registry = Registry::new();
    for (name, concurrency, modifies) in [
        ("fetch", Concurrency::Shared, false),
        ("exclusive", Concurrency::Exclusive, false),
        ("touches", Concurrency::Shared, true),
    ] {
        registry
            .try_register(Arc::new(External {
                name,
                concurrency,
                modifies,
            }))
            .unwrap();
    }
    let fetch = resolve(&ws, EditMode::default(), &registry, "fetch", &json!({})).unwrap();
    assert!(fetch.is_empty() && !fetch.writes());
    let write = resolve(
        &ws,
        EditMode::default(),
        &registry,
        "write",
        &json!({"path":"a"}),
    );
    assert!(!fetch.conflicts(&write.unwrap()));
    for name in ["exclusive", "touches"] {
        assert!(resolve(&ws, EditMode::default(), &registry, name, &json!({})).is_none());
    }
    for builtin in crate::tools::net::web::tools(&crate::config::types::Config::default()) {
        if matches!(builtin.name(), "web_fetch" | "web_search") {
            assert!(
                external_read(builtin.as_ref(), &json!({})),
                "{}",
                builtin.name()
            );
        }
    }
}

#[test]
fn recorded_changes_touch_footprints_by_lexical_containment() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "a").unwrap();
    std::fs::write(root.path().join("b.rs"), "b").unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let registry = Registry::new();
    let file = resolve(
        &ws,
        EditMode::default(),
        &registry,
        "read",
        &json!({"path":"src/a.rs"}),
    )
    .unwrap();
    let base = ws.root();
    assert!(file.touches(base, &[PathBuf::from("src/a.rs")]));
    assert!(file.touches(base, &[base.join("src/a.rs")]));
    assert!(
        file.touches(base, &[PathBuf::from("./src")]),
        "a removed parent"
    );
    assert!(!file.touches(base, &[PathBuf::from("b.rs")]));
    assert!(!file.touches(base, &[]));
    assert!(!Footprint::default().touches(base, &[PathBuf::from("src/a.rs")]));
}

#[test]
fn workspace_reads_conflict_with_every_write_but_no_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "a").unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let registry = Registry::new();
    let get = |name, args: Value| resolve(&ws, EditMode::default(), &registry, name, &args);
    let all = workspace_read(&ws).unwrap();
    assert!(!all.is_empty() && !all.writes());
    assert!(all.conflicts(&get("write", json!({"path":"src/a.rs"})).unwrap()));
    assert!(all.conflicts(&get("write", json!({"path":"new"})).unwrap()));
    assert!(!all.conflicts(&get("read", json!({"path":"src/a.rs"})).unwrap()));
    assert!(!all.conflicts(&all));
}

#[test]
fn vcs_and_dependency_reads_declare_footprints_and_mutations_stay_unknown() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "a").unwrap();
    std::fs::write(root.path().join("b.rs"), "b").unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let config = crate::config::types::Config::default();
    let mut registry = Registry::new();
    for tool in crate::tools::vcs::git::tools(&config)
        .into_iter()
        .chain(crate::tools::vcs::gh::tools(&config))
        .chain(crate::tools::vcs::glab::tools(&config))
        .chain(crate::tools::knowledge::deps::tools(&config))
    {
        registry.try_register(tool).unwrap();
    }
    let get = |name, args: Value| resolve(&ws, EditMode::default(), &registry, name, &args);
    let write_a = get("write", json!({"path":"src/a.rs"})).unwrap();
    let write_b = get("write", json!({"path":"b.rs"})).unwrap();

    for (name, args) in [
        ("git", json!({"action":"log"})),
        ("git", json!({"action":"show","rev":"HEAD"})),
        ("git", json!({"action":"branches"})),
        ("git", json!({"action":"pr_view","number":1})),
        ("github", json!({"op":"pr_diff","pr":"1"})),
        ("github", json!({"op":"search_code","query":"x"})),
        ("glab_mr", json!({"op":"mr_view","mr":"1"})),
    ] {
        let footprint = get(name, args.clone()).expect("external read");
        assert!(footprint.is_empty(), "{name}: {args}");
    }
    let scoped = get("git", json!({"action":"diff","path":"src/a.rs"})).unwrap();
    assert!(scoped.conflicts(&write_a) && !scoped.conflicts(&write_b));
    for (name, args) in [
        ("git", json!({"action":"status"})),
        ("git", json!({"action":"diff"})),
        ("dep_source", json!({"name":"serde"})),
    ] {
        let footprint = get(name, args.clone()).expect("workspace read");
        assert!(
            footprint.conflicts(&write_a) && footprint.conflicts(&write_b) && !footprint.writes(),
            "{name}: {args}"
        );
    }
    for (name, args) in [
        ("git", json!({"action":"commit","message":"m"})),
        ("git", json!({"action":"push"})),
        ("git", json!({"action":"log","vcs":"svn"})),
        ("git", json!({})),
        ("github", json!({"op":"pr_checkout","pr":"1"})),
        ("github", json!({"op":"pr_create"})),
        ("glab_mr", json!({"op":"mr_merge","mr":"1"})),
    ] {
        assert!(get(name, args.clone()).is_none(), "{name}: {args}");
    }

    // jj snapshots the whole working copy before any command, including `log`.
    std::fs::create_dir(root.path().join(".jj")).unwrap();
    let jj = get("git", json!({"action":"log"})).unwrap();
    assert!(jj.conflicts(&write_b));
    let forced = get("git", json!({"action":"log","vcs":"git"})).unwrap();
    assert!(forced.is_empty());
}

#[test]
fn batched_writes_conflict_with_each_destination_but_not_disjoint_reads() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    for path in ["a", "b", "c"] {
        std::fs::write(root.path().join(path), "source").unwrap();
    }
    let registry = Registry::new();
    let mode = EditMode::default();
    let batch = resolve(
        &ws,
        mode,
        &registry,
        "write",
        &json!({"files":[{"path":"a","content":"x"},{"path":"b","content":"y"}]}),
    )
    .unwrap();
    for path in ["a", "b"] {
        let read = resolve(&ws, mode, &registry, "read", &json!({"path":path})).unwrap();
        assert!(batch.conflicts(&read));
    }
    let read = resolve(&ws, mode, &registry, "read", &json!({"path":"c"})).unwrap();
    assert!(!batch.conflicts(&read));
}
