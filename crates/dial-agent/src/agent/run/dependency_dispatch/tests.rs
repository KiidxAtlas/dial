use super::*;

type EffectProbe = Arc<dyn Fn(&[PathBuf]) -> std::result::Result<(), String> + Send + Sync>;
thread_local! {
    static EFFECT_PROBE: std::cell::RefCell<Option<EffectProbe>> = const { std::cell::RefCell::new(None) };
}
pub(in crate::agent::run) fn effect_probe() -> Option<EffectProbe> {
    EFFECT_PROBE.with(|slot| slot.borrow().clone())
}
struct ProbeScope;
impl Drop for ProbeScope {
    fn drop(&mut self) {
        EFFECT_PROBE.with(|slot| *slot.borrow_mut() = None);
    }
}

fn call(name: &str, path: &str) -> ToolCall {
    ToolCall {
        id: path.into(),
        name: name.into(),
        arguments: json!({"path":path,"content":"updated\n"}),
        malformed_arguments: false,
    }
}

fn backend() -> Backend {
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

fn agent<'a>(config: &'a Config, root: &Path) -> Agent<'a> {
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend(), &version, root, None, None).unwrap()
}

#[test]
fn native_footprints_reject_conflicts_and_unknown_or_aliased_resources() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), "initial").unwrap();
    std::fs::write(root.path().join("b"), "initial").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mode = crate::config::types::EditMode::default();
    let read = footprint(&workspace, mode, &call("read", "a")).unwrap();
    let write = footprint(&workspace, mode, &call("write", "a")).unwrap();
    assert!(read.conflicts(&write));
    assert!(!read.conflicts(&read));
    assert!(!write.conflicts(&footprint(&workspace, mode, &call("write", "b")).unwrap()));
    for (name, path) in [
        ("bash", "a"),
        ("read", "."),
        ("write", "../outside"),
        ("edit", "a"),
        ("write", "missing/child"),
        ("write", ".gitignore"),
    ] {
        assert!(
            footprint(&workspace, mode, &call(name, path)).is_none(),
            "{name}: {path}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a", root.path().join("alias")).unwrap();
        assert!(footprint(&workspace, mode, &call("write", "alias")).is_none());
        std::fs::hard_link(root.path().join("a"), root.path().join("hard")).unwrap();
        assert!(footprint(&workspace, mode, &call("write", "a")).is_none());
    }
}

#[test]
fn independent_prefix_stops_at_first_conflict_or_unknown_effect() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.task.max_concurrency = 4;
    let mut agent = agent(&config, root.path());
    let calls = [
        call("write", "a"),
        call("write", "b"),
        call("read", "a"),
        call("write", "c"),
    ];
    assert_eq!(agent.dependency_wave(&calls).len(), 2);
    let calls = [call("write", "a"), call("bash", "b"), call("write", "c")];
    assert_eq!(agent.dependency_wave(&calls).len(), 1);
}

#[test]
fn disjoint_native_writes_keep_order_and_denials_have_no_effects() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Auto;
    let state = tempfile::tempdir().unwrap();
    let checkpoints = Arc::new(
        Checkpoints::new(state.path().join("session"), agent.workspace.root().into()).unwrap(),
    );
    checkpoints.begin_turn("wave").unwrap();
    agent.workspace = agent.workspace.clone().with_observer(Arc::new(
        crate::agent::base::helpers::CheckpointObserver(checkpoints.clone()),
    ));
    agent.checkpoints = Some(checkpoints.clone());
    // Pause at the real native mutation callback, after each worker has computed its paths.
    // A serial worker implementation cannot make both arrive and fails within the deadline.
    let arrivals = Arc::new((
        std::sync::Mutex::new(std::collections::BTreeSet::new()),
        std::sync::Condvar::new(),
    ));
    let observed = arrivals.clone();
    EFFECT_PROBE.with(|slot| {
        *slot.borrow_mut() = Some(Arc::new(move |paths| {
            let (lock, changed) = &*observed;
            let mut seen = lock.lock().unwrap();
            seen.extend(paths.iter().cloned());
            changed.notify_all();
            let (seen, _) = changed
                .wait_timeout_while(seen, Duration::from_secs(3), |seen| seen.len() < 2)
                .unwrap();
            if seen.len() < 2 {
                return Err("independent native workers did not overlap".into());
            }
            Ok(())
        }))
    });
    let probe = ProbeScope;
    let calls = [call("write", "a.txt"), call("write", "b.txt")];
    let results = agent.dispatch_dependency_wave(&mut backend(), &calls, &mut NullUi);
    drop(probe);
    assert_eq!(arrivals.0.lock().unwrap().len(), 2);
    assert_eq!(results.len(), 2);
    for (call, (output, _)) in calls.iter().zip(&results) {
        assert!(!output.is_error, "{}: {}", call.id, output.text);
        assert_eq!(
            std::fs::read_to_string(root.path().join(&call.id)).unwrap(),
            "updated\n"
        );
    }
    for call in &calls {
        let plan = checkpoints
            .revert_plan(&root.path().join(&call.id), false)
            .unwrap();
        assert_eq!(
            plan.content, None,
            "new file {} must revert to absence",
            call.id
        );
    }
    // A later wave must open a new per-path revision, not merge into the drained wave.
    assert!(!tools::execute(&agent.workspace, "read", &json!({"path":"b.txt"}), 30).is_error);
    let mut next = call("write", "b.txt");
    next.arguments["content"] = json!("next revision\n");
    let results = agent.dispatch_dependency_wave(
        &mut backend(),
        &[next, call("write", "c.txt")],
        &mut NullUi,
    );
    assert!(
        results.iter().all(|(output, _)| !output.is_error),
        "{results:?}"
    );
    assert_eq!(
        checkpoints
            .revert_plan(&root.path().join("b.txt"), false)
            .unwrap()
            .content,
        Some(b"updated\n".to_vec())
    );
    assert_eq!(
        checkpoints
            .revert_plan(&root.path().join("a.txt"), false)
            .unwrap()
            .content,
        None
    );
    // Outside edits after settlement must not be attributed to the wave or silently reverted.
    std::fs::write(root.path().join("a.txt"), "external edit").unwrap();
    assert!(
        checkpoints
            .revert_plan(&root.path().join("a.txt"), false)
            .is_err()
    );
    agent.plan_mode = true;
    let denied = call("write", "denied.txt");
    let results = agent.dispatch_dependency_wave(&mut backend(), &[denied], &mut NullUi);
    assert!(results[0].0.is_error);
    assert!(!root.path().join("denied.txt").exists());
}
