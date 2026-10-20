use super::*;
use crate::agent::base::helpers::CheckpointObserver;

fn hermetic_agent<'a>(config: &'a Config, root: &Path, backend: Backend) -> Agent<'a> {
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend, &version, root, None, None).unwrap()
}

fn offline_backend() -> Backend {
    Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
}

fn attach(agent: &mut Agent<'_>, state: &Path) -> Arc<Checkpoints> {
    let checkpoints =
        Arc::new(Checkpoints::new(state.join("session"), agent.workspace.root().into()).unwrap());
    agent.workspace = agent
        .workspace
        .clone()
        .with_observer(Arc::new(CheckpointObserver(checkpoints.clone())));
    agent.checkpoints = Some(checkpoints.clone());
    checkpoints
}

fn replace_config() -> Config {
    let mut config = Config::default();
    config.edit_mode_override = Some(crate::config::types::EditMode::Replace);
    config
}

/// One edit tool call as the turn loop runs it: read, commit, then settle the journal.
fn edit(agent: &Agent<'_>, old: &str, new: &str) {
    let read = tools::execute(&agent.workspace, "read", &json!({"path":"a.txt"}), 30);
    assert!(!read.is_error, "{}", read.text);
    let out = tools::execute(
        &agent.workspace,
        "edit",
        &json!({"path":"a.txt","old_string":old,"new_string":new}),
        30,
    );
    assert!(!out.is_error, "{}", out.text);
    agent.checkpoints.as_ref().unwrap().settle_changes();
}

/// Through `dispatch`, so routing and the approval gate are the ones a model call takes.
fn revert(agent: &mut Agent<'_>, arguments: Value) -> ToolOutput {
    let mut backend = agent.backend.take().unwrap();
    let out = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: "call_revert".into(),
            name: NAME.into(),
            arguments,
            malformed_arguments: false,
        },
        &mut NullUi,
    );
    agent.backend = Some(backend);
    out
}

#[test]
fn restores_the_latest_change_then_the_turn_start_and_undo_still_applies() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "zero\n").unwrap();
    let config = replace_config();
    let mut agent = hermetic_agent(&config, root.path(), offline_backend());
    let checkpoints = attach(&mut agent, state.path());
    checkpoints.begin_turn("t").unwrap();
    edit(&agent, "zero", "one");
    edit(&agent, "one", "two");

    let out = revert(&mut agent, json!({"path":"a.txt"}));
    assert!(!out.is_error, "{}", out.text);
    let file = root.path().join("a.txt");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    assert_eq!(
        out.execution.changed_paths,
        vec![file.canonicalize().unwrap()],
        "the revert reports its committed path, so mutation accounting sees it"
    );
    let out = revert(&mut agent, json!({"path":"a.txt","to":"turn_start"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "zero\n");

    // Each revert was a checkpointed change of this turn, so /undo restores the turn's start.
    edit(&agent, "zero", "three");
    checkpoints.rollback(1).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "zero\n");
}

#[test]
fn refuses_outside_changes_and_files_without_a_journaled_change() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "zero\n").unwrap();
    std::fs::write(root.path().join("b.txt"), "untouched\n").unwrap();
    let config = replace_config();
    let mut agent = hermetic_agent(&config, root.path(), offline_backend());
    let checkpoints = attach(&mut agent, state.path());
    checkpoints.begin_turn("t").unwrap();

    let out = revert(&mut agent, json!({"path":"b.txt"}));
    assert!(out.is_error);
    assert!(out.text.contains("no checkpointed change"), "{}", out.text);

    edit(&agent, "zero", "one");
    std::fs::write(root.path().join("a.txt"), "shell\n").unwrap();
    let out = revert(&mut agent, json!({"path":"a.txt"}));
    assert!(out.is_error);
    assert!(out.text.contains("outside Dial"), "{}", out.text);
    assert!(out.execution.changed_paths.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "shell\n"
    );
}

#[test]
fn edit_deny_rules_cover_reverts() {
    let root = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "zero\n").unwrap();
    let config = replace_config();
    let mut agent = hermetic_agent(&config, root.path(), offline_backend());
    let checkpoints = attach(&mut agent, state.path());
    checkpoints.begin_turn("t").unwrap();
    edit(&agent, "zero", "one");
    agent
        .always
        .push(crate::agent::safety::approval::PermissionRule {
            effect: crate::agent::safety::approval::Effect::Deny,
            tool: "edit".into(),
            command_prefix: Vec::new(),
            paths: vec!["a.txt".into()],
        });
    let out = revert(&mut agent, json!({"path":"a.txt"}));
    assert!(out.is_error);
    assert_eq!(
        crate::tools::ToolStatus::from_output(&out),
        crate::tools::ToolStatus::Blocked
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "one\n"
    );
}
