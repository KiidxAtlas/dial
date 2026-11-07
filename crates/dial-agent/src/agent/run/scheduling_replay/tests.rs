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
    assert!(root.path().read_dir().unwrap().next().is_none());
}
