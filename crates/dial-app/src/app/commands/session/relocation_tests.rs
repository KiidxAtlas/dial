use super::*;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, App) {
    let (home, project, layout, mut config) = crate::app::tests::config();
    let model = layout.user_dir.join("test-model");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("config.json"), "{}").unwrap();
    std::fs::write(model.join("model.safetensors"), "weights").unwrap();
    config.model = model.to_string_lossy().into_owned();
    config.use_case = "test".into();
    let mut overlay = toml::Table::new();
    overlay.insert("model".into(), toml::Value::String(config.model.clone()));
    let app = App::new(
        layout,
        config,
        overlay,
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    (home, project, app)
}

#[test]
fn relocation_then_rebuild_keeps_the_destination_authoritative() {
    let (_home, _project, mut app) = fixture();
    let destination = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let mut session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    session
        .append(serde_json::json!({"role": "user", "content": "keep this history"}))
        .unwrap();
    let history = session.messages();
    let id = session.id().to_owned();
    let original = session.path().unwrap().to_owned();
    let (session, layout, config) = {
        let mut agent = app.make_agent(session).unwrap();
        agent
            .add_workspace_roots(&[extra.path().to_owned()])
            .unwrap();
        agent.persist_workspace_roots().unwrap();
        agent.set_plan_mode(true);
        let (outcome, config) =
            move_project(&app, &mut agent, destination.path().to_str().unwrap()).unwrap();
        assert!(matches!(outcome, MoveOutcome::Moved(_)));
        assert!(
            agent.plan_mode(),
            "same-session relocation must not reset modes"
        );
        let layout = agent.layout().unwrap().clone();
        (
            std::mem::replace(agent.session_mut(), Session::ephemeral()),
            layout,
            config.unwrap(),
        )
    };
    assert!(!original.exists());
    assert!(app.adopt_project(layout.clone(), config));
    let reloaded = app.reload().unwrap();
    assert_eq!(reloaded.state_dir, reloaded.learning_dir(&layout));
    assert_eq!(reloaded.state_dir, app.config.state_dir);
    assert!(reloaded.state_dir.starts_with(&layout.state_dir));
    let rebuilt = app.make_agent(session).unwrap();
    assert_eq!(rebuilt.session().id(), id);
    assert_eq!(rebuilt.session().messages(), history);
    assert_eq!(rebuilt.workspace().root(), layout.project_root);
    assert_eq!(rebuilt.layout().unwrap().state_dir, layout.state_dir);
    // Attached roots belong to the old project; relocation deliberately revokes them.
    assert!(rebuilt.workspace().additional_roots().is_empty());
    assert!(rebuilt.session().additional_dirs().is_empty());
    assert!(
        rebuilt
            .session()
            .path()
            .unwrap()
            .starts_with(layout.sessions_dir())
    );
}

#[test]
fn failed_reroot_restores_the_original_writer_and_workspace() {
    let (_home, _project, app) = fixture();
    let destination = tempfile::tempdir().unwrap();
    let target = Layout::with_user_dir(destination.path(), app.layout.user_dir.clone()).unwrap();
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    let original = session.path().unwrap().to_owned();
    let mut agent = app.make_agent(session).unwrap();
    // Checkpoint setup is fallible and must happen before publishing workspace/layout/roots.
    std::fs::create_dir_all(&target.state_dir).unwrap();
    std::fs::write(target.checkpoints_dir(), "not a directory").unwrap();
    assert!(move_session(&mut agent, destination.path().to_str().unwrap()).is_err());
    assert_eq!(agent.session().path().unwrap(), original);
    assert_eq!(agent.workspace().root(), app.layout.project_root);
    assert_eq!(
        agent.layout().unwrap().project_root,
        app.layout.project_root
    );
    assert!(original.exists());
    // Release the live writer first: a held writer makes any reopen look contended, not moved.
    drop(agent);
    // The rolled-back ledger entry must not make the original look moved: it reopens in place.
    assert!(matches!(
        Session::open_or_fork(&app.layout, &original).unwrap(),
        crate::storage::session::Resumed::Owned(_)
    ));
    assert!(Session::list(&target, 10).unwrap_or_default().is_empty());
}

#[test]
fn rejected_destination_validation_leaves_the_session_untouched() {
    let (_home, _project, app) = fixture();
    let destination = tempfile::tempdir().unwrap();
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    let original = session.path().unwrap().to_owned();
    let mut agent = app.make_agent(session).unwrap();
    assert!(
        move_session_checked(&mut agent, destination.path().to_str().unwrap(), |_| bail!(
            "invalid destination"
        ))
        .is_err()
    );
    assert_eq!(agent.session().path().unwrap(), original);
    assert_eq!(agent.workspace().root(), app.layout.project_root);
    assert!(original.exists());
}

#[test]
fn terminal_switch_preserves_launch_roots_but_replaces_session_only_grants() {
    let (_home, _project, mut app) = fixture();
    let launch = tempfile::tempdir().unwrap();
    let outgoing = tempfile::tempdir().unwrap();
    let incoming = tempfile::tempdir().unwrap();
    app.add_dirs = vec![launch.path().canonicalize().unwrap()];
    let session = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    let mut agent = app.make_agent(session).unwrap();
    agent
        .add_workspace_roots(&[outgoing.path().to_owned()])
        .unwrap();
    agent.persist_workspace_roots().unwrap();
    let mut next = Session::create(
        &app.layout,
        SessionMeta {
            model: app.config.model.clone(),
        },
    )
    .unwrap();
    next.set_additional_dirs(&[incoming.path().to_owned()])
        .unwrap();
    let old = app.swap_agent_session(&mut agent, next).unwrap();
    let roots = agent.workspace().additional_roots();
    assert!(roots.contains(&launch.path().canonicalize().unwrap()));
    assert!(roots.contains(&incoming.path().canonicalize().unwrap()));
    assert!(!roots.contains(&outgoing.path().canonicalize().unwrap()));
    let previous = app.swap_agent_session(&mut agent, old).unwrap();
    assert!(
        agent
            .workspace()
            .additional_roots()
            .contains(&outgoing.path().canonicalize().unwrap())
    );
    assert!(
        !agent
            .workspace()
            .additional_roots()
            .contains(&incoming.path().canonicalize().unwrap())
    );
    drop(previous);
}
