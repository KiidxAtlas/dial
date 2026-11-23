use super::*;
use serde_json::json;

#[test]
fn recorded_prefixes_use_the_live_conflict_planner_without_executing_effects() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let config = Config::default();
    let call = |name: &str, path: &str| ToolCall {
        id: path.into(),
        name: name.into(),
        arguments: json!({"path":path,"content":"new"}),
        malformed_arguments: false,
    };
    let calls = vec![
        call("write", "a"),
        call("write", "b"),
        call("read", "a"),
        call("bash", "unknown"),
        call("write", "c"),
    ];
    let report = replay_scheduling(&workspace, &config, &Registry::new(), &[calls]);
    assert_eq!(report.calls, 5);
    assert_eq!(report.read_batching_only.groups, 5);
    assert_eq!(report.resource_dispatch.groups, 4);
    assert_eq!(report.resource_dispatch.grouped_calls, 2);
    assert_eq!(report.resource_barriers["unknown_resources"], 3); // prefix ending at absent read, absent read itself, shell
    assert_eq!(report.resource_barrier_tools["unknown_resources:read"], 2);
    assert_eq!(report.resource_barrier_tools["unknown_resources:bash"], 1);
    assert_eq!(report.resource_barrier_tools.len(), 2); // response_end names no tool
    assert!(root.path().read_dir().unwrap().next().is_none());
}

#[test]
fn recorded_file_handles_expand_like_the_live_wave() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "old").unwrap();
    let config = Config::default();
    let calls = vec![
        ToolCall {
            id: "1".into(),
            name: "write".into(),
            arguments: json!({"path":"b","content":"new"}),
            malformed_arguments: false,
        },
        ToolCall {
            id: "2".into(),
            name: "read".into(),
            arguments: json!({"path":"@f1"}),
            malformed_arguments: false,
        },
    ];

    // Without the recorded registry the handle is unknown and closes the prefix, as it would live.
    let fresh = Workspace::new(root.path()).unwrap();
    let report = replay_scheduling(
        &fresh,
        &config,
        &Registry::new(),
        std::slice::from_ref(&calls),
    );
    // Closes the prefix after `write`, then its own single-call prefix.
    assert_eq!(report.resource_barrier_tools["file_reference:read"], 2);
    assert_eq!(report.resource_dispatch.grouped_calls, 0);

    // With the session's handle registered, the read resolves to `a`, disjoint from `b`.
    let restored = Workspace::new(root.path()).unwrap();
    assert_eq!(restored.file_handle("a").unwrap(), "@f1");
    let mut combined = replay_scheduling(&restored, &config, &Registry::new(), &[calls]);
    assert_eq!(combined.resource_dispatch.groups, 1);
    assert_eq!(combined.resource_dispatch.grouped_calls, 2);
    assert_eq!(combined.resource_barriers["response_end"], 1);

    combined.extend(report);
    assert_eq!(combined.responses, 2);
    assert_eq!(combined.calls, 4);
    assert_eq!(combined.resource_barrier_tools["file_reference:read"], 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a")).unwrap(),
        "old"
    );
    assert!(!root.path().join("b").exists());
}
