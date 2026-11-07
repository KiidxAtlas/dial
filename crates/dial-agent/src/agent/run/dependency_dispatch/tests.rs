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
    assert_eq!(agent.dependency_wave(&calls).calls.len(), 2);
    let calls = [call("write", "a"), call("bash", "b"), call("write", "c")];
    assert_eq!(agent.dependency_wave(&calls).calls.len(), 1);
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
    let results = agent.dispatch_dependency_wave(&mut backend(), &calls, &mut NullUi, None);
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
        None,
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
    let results = agent.dispatch_dependency_wave(&mut backend(), &[denied], &mut NullUi, None);
    assert!(results[0].0.is_error);
    assert!(!root.path().join("denied.txt").exists());
}

#[test]
fn failed_mutations_skip_only_dependent_or_opaque_work_not_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let registry = crate::tools::integrations::ext::Registry::default();
    let mode = crate::config::types::EditMode::default();
    let mut prerequisites = BatchPrerequisites::default();
    prerequisites.observe(
        &workspace,
        mode,
        &call("write", "a"),
        &ToolOutput::err("refused").with_effects(tools::surface::outcome::Effects::None),
        &registry,
    );
    for (name, args, skipped) in [
        ("write", json!({"path":"a","content":"x"}), true),
        ("write", json!({"path":"b","content":"x"}), false),
        ("read", json!({"path":"a"}), false),
        ("grep", json!({"pattern":"cause"}), false),
        ("bash", json!({"command":"python3 a"}), true),
        ("bash", json!({"command":"python3 --version"}), false),
        ("verify", json!({"targets":["build"]}), true),
        ("bg", json!({"action":"start","command":"python3 a"}), true),
        ("bg", json!({"action":"output","name":"job"}), false),
        ("tool_search", json!({"query":"debugger"}), false),
    ] {
        let action = ToolCall {
            id: "later".into(),
            name: name.into(),
            arguments: args,
            malformed_arguments: false,
        };
        let result = prerequisites.rejection(&workspace, mode, &registry, &action);
        assert_eq!(result.is_some(), skipped, "{action:?}");
        if let Some(output) = result {
            assert_eq!(output.execution.status, Some(tools::ToolStatus::Skipped));
            assert_eq!(
                output.execution.effects,
                tools::surface::outcome::Effects::None
            );
            assert!(output.execution.changed_paths.is_empty());
        }
    }
    assert!(!root.path().join("a").exists());
    // A fresh model request can deliberately diagnose or correct the failed prerequisite.
    assert!(
        BatchPrerequisites::default()
            .rejection(&workspace, mode, &registry, &call("write", "a"))
            .is_none()
    );
}

#[test]
fn checks_close_the_wave_after_the_verified_mutation() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = agent(&config, root.path());
    let mut last = call("write", "b");
    last.arguments["verify"] = json!(["candidate"]);
    let calls = [call("write", "a"), last, call("write", "c")];
    assert_eq!(agent.dependency_wave(&calls).calls.len(), 2);
    assert_eq!(
        agent.efficiency_metrics.scheduling.barriers["verification"],
        1
    );
}

#[test]
fn bookkeeping_and_scoped_reads_join_native_groups_but_deliverable_changes_stay_boundaries() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("a"), "evidence").unwrap();
    std::fs::write(root.path().join("b"), "evidence").unwrap();
    let mut config = Config::default();
    config.task.max_concurrency = 8;
    let mut agent = agent(&config, root.path());
    let mut read = call("read", "unused");
    read.arguments = json!({"paths":["a","b"]});
    let mut search = call("grep", "src");
    search.arguments["pattern"] = json!("source");
    let mut todo = call("todo", "unused");
    todo.arguments = json!({"op":"view"});
    let calls = [call("write", "new"), todo, read, search];
    assert_eq!(agent.dependency_wave(&calls).calls.len(), 4);
    let mut scope = call("working_state", "unused");
    scope.arguments = json!({"action":"patch","deliverables":[]});
    assert_eq!(
        agent
            .dependency_wave(&[calls[0].clone(), scope, calls[2].clone()])
            .calls
            .len(),
        1
    );
}

#[test]
fn grouped_verification_runs_once_after_all_source_epochs_and_bookkeeping_settle() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.task.max_concurrency = 4;
    let command = "test -f a && test -f b && printf 'checked\\n' >> checks";
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "candidate".into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
    let mut agent = agent(&config, root.path());
    agent.hermetic = false;
    let mut todo = call("todo", "todo");
    todo.arguments = json!({"op":"view"});
    let mut last = call("write", "b");
    last.arguments["verify"] = json!(["candidate"]);
    let calls = [call("write", "a"), todo, last];
    agent.verify_batch.begin(&calls);
    assert_eq!(agent.dependency_wave(&calls).calls.len(), 3);
    let mut results = agent.dispatch_dependency_wave(&mut backend(), &calls, &mut NullUi, None);
    for (call, (out, _)) in calls.iter().zip(&mut results) {
        assert!(!out.is_error, "{}: {}", call.name, out.text);
        agent.settle_tool_mutation(call, out); // ordinary settlement must not stale the checks
    }
    assert_eq!(agent.step_checks.mutations, 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("checks")).unwrap(),
        "checked\n"
    );
    assert_eq!(
        agent
            .step_checks
            .evidence
            .current_status(command, agent.step_checks.revision()),
        "passed"
    );
    assert_eq!(
        agent.efficiency_metrics.scheduling.groups["native"].sizes[&2],
        1
    );
}

fn plan(
    workspace: &Workspace,
    registry: &crate::tools::integrations::ext::Registry,
    policy: WavePolicy,
    calls: &[ToolCall],
) -> Plan {
    plan_wave(
        calls,
        WaveScope {
            workspace,
            mode: crate::config::types::EditMode::default(),
            registry,
            hooks: &[],
            limit: 8,
        },
        policy,
        |call| Ok(call.clone()),
    )
}

#[test]
fn one_wave_admission_applies_the_child_role_policy() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let registry = crate::tools::integrations::ext::Registry::new();
    let mut todo = call("todo", "unused");
    todo.arguments = json!({"op":"view"});
    let calls = [call("write", "a"), todo, call("write", "b")];
    let parent = plan(&workspace, &registry, WavePolicy::PARENT, &calls);
    assert_eq!(parent.calls.len(), 3);
    assert!(parent.dispatchable() && parent.writes());
    let child = plan(&workspace, &registry, WavePolicy::CHILD, &calls);
    assert_eq!(child.calls.len(), 1);
    assert_eq!(child.barrier, "worker_admission");

    let mut verified = call("write", "a");
    verified.arguments["verify"] = json!(["candidate"]);
    let calls = [call("write", "b"), verified, call("write", "c")];
    let parent = plan(&workspace, &registry, WavePolicy::PARENT, &calls);
    assert_eq!((parent.calls.len(), parent.barrier), (2, "verification"));
    let child = plan(&workspace, &registry, WavePolicy::CHILD, &calls);
    assert_eq!((child.calls.len(), child.barrier), (1, "worker_admission"));
}

#[test]
fn effect_free_external_reads_form_a_wave_but_pure_read_groups_do_not() {
    let root = tempfile::tempdir().unwrap();
    for file in ["a", "b"] {
        std::fs::write(root.path().join(file), "source").unwrap();
    }
    let workspace = Workspace::new(root.path()).unwrap();
    let mut registry = crate::tools::integrations::ext::Registry::new();
    for tool in crate::tools::net::web::tools(&Config::default()) {
        registry.try_register(tool).unwrap();
    }
    let fetch = |url: &str| ToolCall {
        id: url.into(),
        name: "web_fetch".into(),
        arguments: json!({"url":url}),
        malformed_arguments: false,
    };
    let fetches = plan(
        &workspace,
        &registry,
        WavePolicy::PARENT,
        &[
            fetch("https://a.example"),
            fetch("https://b.example"),
            call("read", "a"),
        ],
    );
    assert_eq!(fetches.calls.len(), 3);
    assert!(fetches.external() && !fetches.writes() && fetches.dispatchable());
    let child = plan(
        &workspace,
        &registry,
        WavePolicy::CHILD,
        &[fetch("https://a.example"), fetch("https://b.example")],
    );
    assert!(child.dispatchable());
    let reads = plan(
        &workspace,
        &registry,
        WavePolicy::PARENT,
        &[call("read", "a"), call("read", "b")],
    );
    assert_eq!(reads.calls.len(), 2);
    assert!(
        !reads.dispatchable(),
        "pure reads keep the parallel-read path"
    );
}

/// An approval-gated, workspace-inert tool: both calls must be in flight at once to succeed.
struct Rendezvous {
    name: &'static str,
    arrived: Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
}
impl crate::tools::integrations::ext::Tool for Rendezvous {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object","properties":{"url":{"type":"string"}}}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn concurrency(&self, _: &Value) -> crate::tools::integrations::ext::Concurrency {
        crate::tools::integrations::ext::Concurrency::Shared
    }
    fn resources(
        &self,
        _: &Workspace,
        _: &Value,
    ) -> Option<crate::tools::surface::resources::Access> {
        Some(Default::default())
    }
    fn call(&self, _: &crate::tools::integrations::ext::ToolContext, _: &Value) -> ToolOutput {
        let (count, ready) = &*self.arrived;
        let mut arrived = count.lock().unwrap();
        *arrived += 1;
        ready.notify_all();
        let (arrived, timeout) = ready
            .wait_timeout_while(arrived, Duration::from_secs(5), |arrived| *arrived < 2)
            .unwrap();
        if timeout.timed_out() {
            ToolOutput::err(format!("ran alone ({} arrived)", *arrived))
        } else {
            ToolOutput::ok("fetched".into())
        }
    }
}

#[derive(Default)]
struct Approvals(Vec<String>);
impl Ui for Approvals {
    fn approve(&mut self, tool: &str, _summary: &str) -> crate::agent::ui::surface::Decision {
        self.0.push(tool.to_owned());
        if tool == "fetch_denied" {
            crate::agent::ui::surface::Decision::Deny
        } else {
            crate::agent::ui::surface::Decision::Allow
        }
    }
}

#[test]
fn authorized_external_reads_run_together_and_a_denied_one_never_starts() {
    let root = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut agent = agent(&config, root.path());
    agent.hermetic = false;
    agent.approval = crate::config::types::Approval::Ask;
    let arrived = Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new()));
    for name in ["fetch_ok", "fetch_denied"] {
        agent
            .ext
            .try_register(Arc::new(Rendezvous {
                name,
                arrived: arrived.clone(),
            }))
            .unwrap();
    }
    let fetch = |id: &str, name: &str| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({"url":"https://example.test"}),
        malformed_arguments: false,
    };
    let calls = [
        fetch("first", "fetch_ok"),
        fetch("denied", "fetch_denied"),
        fetch("second", "fetch_ok"),
    ];
    let plan = agent.dependency_wave(&calls);
    assert_eq!(plan.calls.len(), 3);
    assert!(plan.dispatchable() && plan.external());
    let mut ui = Approvals::default();
    let results = agent.dispatch_dependency_wave(&mut backend(), &calls, &mut ui, None);
    assert_eq!(
        ui.0,
        ["fetch_ok", "fetch_denied", "fetch_ok"],
        "serial, model-order approval"
    );
    assert_eq!(results[0].0.text, "fetched");
    assert_eq!(results[2].0.text, "fetched");
    assert!(results[1].0.is_error);
    assert_eq!(
        results[1].0.execution.status,
        Some(tools::ToolStatus::Blocked)
    );
    assert_eq!(*arrived.0.lock().unwrap(), 2, "the denied call never ran");
}
