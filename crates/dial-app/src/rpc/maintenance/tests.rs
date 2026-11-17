use super::*;
use crate::storage::session::{Session, SessionMeta};

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    (project, home, layout)
}

fn code(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .unwrap()
        .code
}

#[test]
fn schedules_require_consent_and_preserve_explicit_tool_access() {
    let (_project, _home, layout) = layout();
    for params in [
        json!({"prompt":"review","every":"1h"}),
        json!({"prompt":"review","every":"1h","approve_all":true}),
        json!({"prompt":"review","every":"1h","confirm":"yes"}),
        json!({"prompt":"review","every":"1h","confirm":true,"approval":"auto"}),
        json!({"prompt":"review","every":"1s","confirm":true}),
    ] {
        assert_eq!(
            code(schedule_add(&layout, &params).unwrap_err()),
            ErrorCode::InvalidParams,
            "{params}"
        );
        assert!(Store::new(&layout).unwrap().load().unwrap().is_empty());
    }
    for (id, extra, approval, allow) in [
        ("safe", json!({}), "readonly", vec![]),
        (
            "tools",
            json!({"allow":["read,grep", "read"]}),
            "readonly",
            vec!["read", "grep"],
        ),
        ("all", json!({"approve_all":true}), "auto", vec![]),
    ] {
        let mut params = json!({"id":id,"prompt":"review","every":"1h","confirm":true});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(schedule_add(&layout, &params).unwrap()["id"], id);
        let entries = Store::new(&layout).unwrap().load().unwrap();
        let entry = entries.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(entry.approval, approval);
        assert_eq!(entry.allow, allow);
    }
    let listed = schedule_list(&layout, &json!({})).unwrap();
    assert_eq!(listed["schedules"].as_array().unwrap().len(), 3);
    assert!(listed["schedules"][0]["last_run"].is_null());
    assert_eq!(
        code(schedule_remove(&layout, &json!({"id":"safe"})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    schedule_remove(&layout, &json!({"id":"safe","confirm":true})).unwrap();
    assert_eq!(
        code(schedule_remove(&layout, &json!({"id":"safe","confirm":true})).unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(schedule_status(&layout, &json!({})).unwrap()["entries"], 2);
}

#[test]
fn scheduler_status_tracks_the_real_lease_and_releases_its_probe() {
    let (_project, _home, layout) = layout();
    assert_eq!(
        schedule_status(&layout, &json!({})).unwrap()["running"],
        false
    );
    std::fs::create_dir_all(&layout.state_dir).unwrap();
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(layout.state_dir.join("schedule.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    assert_eq!(
        schedule_status(&layout, &json!({})).unwrap()["running"],
        true
    );
    drop(lease);
    assert_eq!(
        schedule_status(&layout, &json!({})).unwrap()["running"],
        false
    );
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(layout.state_dir.join("schedule.lock"))
        .unwrap();
    lease.try_lock().unwrap();
}

#[test]
fn archive_returns_owner_records_and_rejects_unsupported_filters() {
    let (_project, _home, layout) = layout();
    let mut session = Session::create(
        &layout,
        SessionMeta {
            model: "test".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"user","content":"archived prompt"}))
        .unwrap();
    let view = archive(
        &layout,
        &json!({"view":"sessions","project":layout.project_root}),
    )
    .unwrap();
    assert_eq!(view["records"][0]["id"], session.id());
    assert!(view["text"].is_string());
    for params in [
        json!({"view":"projects","query":"x"}),
        json!({"view":"sessions","query":"x"}),
        json!({"view":"delete"}),
        json!({"limit":0}),
        json!({"limit":501}),
        json!({"unknown":true}),
    ] {
        assert_eq!(
            code(archive(&layout, &params).unwrap_err()),
            ErrorCode::InvalidParams,
            "{params}"
        );
    }
}

fn with_agent(f: impl FnOnce(&Config, &Arc<AtomicBool>, &mut Agent<'_>)) {
    let (_project, _home, layout) = layout();
    let mut config = Config {
        state_dir: layout.state_dir.clone(),
        compute_locks: false,
        ..Config::default()
    };
    config.codebase.enabled = false;
    config.memory.backend = crate::config::memory_backend::MemoryBackendId::Off;
    let backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let interrupt = Arc::new(AtomicBool::new(false));
    let session = Session::create(
        &layout,
        SessionMeta {
            model: "test".into(),
        },
    )
    .unwrap();
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        interrupt.clone(),
    )
    .unwrap();
    f(&config, &interrupt, &mut agent);
}

#[test]
fn active_session_label_pack_and_branch_validation_use_the_owners() {
    with_agent(|config, interrupt, agent| {
        agent
            .session_mut()
            .append(json!({"role":"user","content":"keep me"}))
            .unwrap();
        let labeled = label(agent, &json!({"label":"  useful  point "})).unwrap();
        assert_eq!(labeled["label"], "useful point");
        assert!(
            agent
                .session()
                .tree()
                .iter()
                .any(|node| node.label.as_deref() == Some("useful point"))
        );
        label(agent, &json!({"label":""})).unwrap();
        let packed = pack(agent, &json!({})).unwrap();
        let output = std::path::Path::new(packed["path"].as_str().unwrap());
        let original = std::fs::read(output).unwrap();
        assert_eq!(
            code(pack(agent, &json!({})).unwrap_err()),
            ErrorCode::Conflict
        );
        assert_eq!(std::fs::read(output).unwrap(), original);
        let messages = agent.session().messages();
        for params in [
            json!({"turns":1}),
            json!({"turns":0,"confirm":true}),
            json!({"turns":2,"confirm":true}),
        ] {
            assert_eq!(
                code(branch(config, interrupt, agent, &params).unwrap_err()),
                ErrorCode::InvalidParams
            );
            assert_eq!(agent.session().messages(), messages);
        }
        let journal = agent.session().path().unwrap().to_owned();
        for params in [
            json!({"path":"missing"}),
            json!({"path":"missing","confirm":true}),
            json!({"path":"","confirm":true}),
        ] {
            assert_eq!(
                code(move_session(agent, &toml::Table::new(), &params).unwrap_err()),
                ErrorCode::InvalidParams,
            );
            assert_eq!(agent.session().path(), Some(journal.as_path()));
            assert_eq!(agent.session().messages(), messages);
            assert!(journal.exists());
        }
    });
}
