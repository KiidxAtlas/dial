use super::*;

#[test]
fn batch_receipts_preserve_partial_effects_and_refused_entries() {
    let done = ToolOutput::ok("written".into()).with_changes([PathBuf::from("output.bin")]);
    let blocked = ToolOutput::err("denied")
        .with_status(tools::ToolStatus::Blocked)
        .with_effects(crate::runtime::execution::Effects::None);
    let uncertain =
        ToolOutput::err("interrupted").with_effects(crate::runtime::execution::Effects::Unknown);
    let output = code_batch_output(vec![done, blocked, uncertain]);
    assert_eq!(
        output.execution.changed_paths,
        [PathBuf::from("output.bin")]
    );
    assert_eq!(
        output.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    assert!(
        output.is_error,
        "unknown batch effects cannot be hidden by per-entry results"
    );
    let data = output.data.unwrap();
    assert_eq!(data[0]["changed_paths"][0], "output.bin");
    assert_eq!(data[1]["status"], "blocked");
    assert_eq!(data[1]["effects"], "none");
    assert_eq!(data[2]["effects"], "unknown");
    assert_eq!(data[2]["index"], 2);
}

#[test]
fn settled_callback_batches_keep_their_native_accounting_receipt() {
    let mut done = ToolOutput::ok("written".into()).with_changes([PathBuf::from("output.bin")]);
    done.execution.mutation_settled = true;
    let output = code_batch_output(vec![done, ToolOutput::err("later callback failed")]);
    assert!(output.execution.mutation_settled);
    assert_eq!(
        output.execution.changed_paths,
        [PathBuf::from("output.bin")]
    );
    let unsettled = ToolOutput::ok("new write".into()).with_changes([PathBuf::from("other.bin")]);
    assert!(
        !code_batch_output(vec![output, unsettled])
            .execution
            .mutation_settled
    );
}

fn request(messages: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools: vec![json!({"type":"function","function":{"name":"read"}})],
        temperature: 0.2,
        max_tokens: 64,
        seed: Some(3),
    }
}

#[test]
fn unchanged_projection_keeps_the_borrowed_request() {
    let original = request(vec![json!({"role":"user","content":"hi"})]);
    assert!(projected_request(&original, None).is_none());
}

#[test]
fn projected_history_replaces_only_messages() {
    let original = request(vec![json!({"role":"user","content":"old"})]);
    let projected = projected_request(
        &original,
        Some(vec![json!({"role":"user","content":"new"})]),
    )
    .expect("a changed projection produces a request");
    assert_eq!(
        projected.messages,
        vec![json!({"role":"user","content":"new"})]
    );
    assert_eq!(projected.tools, original.tools);
    assert_eq!(projected.temperature, original.temperature);
    assert_eq!(projected.max_tokens, original.max_tokens);
    assert_eq!(projected.seed, original.seed);
    assert_eq!(original.messages[0]["content"], "old");
}

#[test]
fn a_wire_only_read_tool_cannot_authorize_a_history_migration() {
    use crate::llm::tool_offer::{self, Mark};
    let mut original = request(vec![json!({"role":"assistant", "dial_tool_contracts":{
        "call": {"name":"artifact_read","revision":1,"schema_sha256":"legacy"}}})]);
    tool_offer::mark(
        &mut original.tools[0],
        Mark {
            callable: false,
            deferred: false,
        },
    );
    let dir = tempfile::tempdir().unwrap();
    let error = prepare_provider_request(
        &original,
        &Registry::new(),
        &[],
        dir.path(),
        &AtomicBool::new(false),
        &mut |_| panic!("projection must precede hooks"),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("read, which is unavailable"),
        "{error}"
    );
}

#[test]
fn parallel_completion_reaches_the_view_before_a_slow_sibling_and_keeps_call_order() {
    use crate::tools::integrations::ext::{Concurrency, Tool};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    struct ReadProbe(Arc<AtomicBool>);
    impl Tool for ReadProbe {
        fn name(&self) -> &str {
            "completion_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name(),"parameters":{"type":"object","properties":{"slow":{"type":"boolean"}}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn concurrency(&self, _: &Value) -> Concurrency {
            Concurrency::Shared
        }
        fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
            if args["slow"] == true {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !self.0.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                if !self.0.load(Ordering::Acquire) {
                    return ToolOutput::err("fast completion never reached the view");
                }
                ToolOutput::ok("slow observation".into())
            } else {
                ToolOutput::ok("fast observation".into())
            }
        }
    }
    struct View {
        release: Arc<AtomicBool>,
        finished: Vec<String>,
    }
    impl Ui for View {
        fn tool_result_with_id(&mut self, id: &str, _: &str, _: &ToolOutput) {
            self.finished.push(id.to_owned());
            if id == "fast" {
                self.release.store(true, Ordering::Release);
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let mut registry = Registry::new();
    registry.register(Arc::new(ReadProbe(release.clone())));
    let calls = [("slow", true), ("fast", false)].map(|(id, slow)| ToolCall {
        id: id.into(),
        name: "completion_probe".into(),
        arguments: json!({"slow":slow}),
        malformed_arguments: false,
    });
    let mut view = View {
        release,
        finished: Vec::new(),
    };
    let results = parallel_tools(
        ParallelToolContext {
            workspace: &workspace,
            config: &Config::default(),
            ext: &registry,
            interrupt: &AtomicBool::new(false),
            ui: &mut view,
            pending_steering: &mut Vec::new(),
        },
        &calls,
        None,
        None,
    );
    assert_eq!(view.finished, ["fast", "slow"]);
    assert_eq!(
        results
            .iter()
            .map(|(output, _)| output.text.as_str())
            .collect::<Vec<_>>(),
        ["slow observation", "fast observation"]
    );
    assert!(results.iter().all(|(output, _)| !output.is_error));
}

#[test]
fn read_window_refills_while_first_read_is_blocked_without_exceeding_limit() {
    use crate::tools::integrations::ext::Tool;
    use std::sync::atomic::AtomicUsize;
    #[derive(Default)]
    struct Probe {
        active: AtomicUsize,
        peak: AtomicUsize,
        last: AtomicBool,
    }
    impl Tool for Probe {
        fn name(&self) -> &str {
            "window_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name(),"parameters":{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn read_only_hint(&self) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, args: &Value) -> ToolOutput {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let index = args["index"].as_u64().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            if index == 0 {
                while !self.last.load(Ordering::SeqCst) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
            } else if index == 7 {
                self.last.store(true, Ordering::SeqCst);
            } else {
                thread::sleep(Duration::from_millis(15));
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if index == 0 && !self.last.load(Ordering::SeqCst) {
                return ToolOutput::err("read window stalled");
            }
            ToolOutput::ok(index.to_string())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let probe = Arc::new(Probe::default());
    let mut registry = Registry::new();
    registry.try_register(probe.clone()).unwrap();
    let mut config = Config::default();
    config.task.max_concurrency = 2;
    let calls: Vec<_> = (0..8)
        .map(|index| ToolCall {
            id: index.to_string(),
            name: "window_probe".into(),
            arguments: json!({"index":index}),
            malformed_arguments: false,
        })
        .collect();
    struct View;
    impl Ui for View {}
    let results = parallel_tools(
        ParallelToolContext {
            workspace: &workspace,
            config: &config,
            ext: &registry,
            interrupt: &AtomicBool::new(false),
            ui: &mut View,
            pending_steering: &mut Vec::new(),
        },
        &calls,
        None,
        None,
    );
    assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
    for (index, (output, _)) in results.iter().enumerate() {
        assert!(
            !output.is_error && output.text == index.to_string(),
            "{}",
            output.text
        );
    }
}

#[test]
fn rendered_pixels_from_any_tool_become_an_attachment_and_keep_the_description() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(&root.path().join(".art")).unwrap();
    let workspace = Workspace::new(root.path()).unwrap().with_artifacts(store);
    // 1x1 PNG.
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
    let output = |text: &str| ToolOutput {
        execution: Default::default(),
        data: Some(
            json!({"frames":1,"artifact":{"media_type":"image/png","encoding":"base64","data":encoded}}),
        ),
        text: text.into(),
        is_error: false,
    };
    let call = |name: &str| ToolCall {
        id: name.into(),
        name: name.into(),
        arguments: json!({"action":"inspect","path":"clip.mp4"}),
        malformed_arguments: false,
    };
    for tool in ["visual", "video", "evidence", "read", "session_search"] {
        let mut out = output("frame 0 at 0.0s; first frame attached");
        let image = capture_output_image(&workspace, &call(tool), &mut out)
            .unwrap()
            .unwrap_or_else(|| panic!("{tool} pixels must reach the model"));
        assert!(image.is_object(), "{tool}: {image}");
        assert_eq!(out.text, "frame 0 at 0.0s; first frame attached", "{tool}");
        let data = out.data.as_ref().unwrap();
        assert!(data["artifact"]["uri"].is_string() && data["artifact"]["data"].is_null());
    }
    let mut failed = output("boom");
    failed.is_error = true;
    assert!(
        capture_output_image(&workspace, &call("visual"), &mut failed)
            .unwrap()
            .is_none()
    );
    let mut plain = ToolOutput::ok("no pixels".into());
    assert!(
        capture_output_image(&workspace, &call("data"), &mut plain)
            .unwrap()
            .is_none()
    );
}

#[test]
fn recovered_safe_reads_require_human_approval_in_the_main_owner() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    use crate::agent::{
        Decision,
        safety::approval::{CallProvenance, Denial, Effect, PermissionRule},
    };
    struct View(usize);
    impl Ui for View {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.0 += 1;
            Decision::AllowProject
        }
    }
    with_agent("", |agent| {
        let call = ToolCall {
            id: "read".into(),
            name: "read".into(),
            arguments: json!({"path":"file"}),
            malformed_arguments: false,
        };
        agent.approval = Approval::Auto;
        let mut view = View(0);
        assert!(
            agent
                .authorize_call_from(&call, CallProvenance::Structured, &mut view)
                .is_ok()
        );
        assert_eq!(
            view.0, 0,
            "structured safe reads keep their existing shortcut"
        );
        for _ in 0..2 {
            assert!(
                agent
                    .authorize_call_from(&call, CallProvenance::RecoveredText, &mut view)
                    .is_ok()
            );
        }
        assert_eq!(view.0, 2, "recovered approval never becomes reusable");
        assert!(agent.always.is_empty());
        agent.approval = Approval::Yolo;
        assert!(
            agent
                .authorize_call_from(&call, CallProvenance::RecoveredText, &mut view)
                .is_ok()
        );
        assert_eq!(
            view.0, 2,
            "YOLO must not prompt on an eligible recovered call"
        );
        assert!(agent.always.is_empty(), "YOLO must not create grants");
        agent.always.push(PermissionRule {
            effect: Effect::Deny,
            tool: "read".into(),
            command_prefix: vec![],
            paths: vec![],
        });
        for mode in [Approval::Auto, Approval::Yolo] {
            agent.approval = mode;
            assert!(matches!(
                agent.authorize_call_from(&call, CallProvenance::RecoveredText, &mut view),
                Err(Denial::Rule)
            ));
        }
        agent.always.clear();
        agent.interrupt.store(true, Ordering::SeqCst);
        for mode in [Approval::Auto, Approval::Yolo] {
            agent.approval = mode;
            assert!(matches!(
                agent.authorize_call_from(&call, CallProvenance::RecoveredText, &mut view),
                Err(Denial::User)
            ));
        }
        assert_eq!(view.0, 2, "denied and cancelled calls must not prompt");
    });
}

#[test]
fn code_cell_batches_run_read_only_entries_together_and_fall_back_to_serial_callbacks() {
    use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
    use crate::agent::safety::rules::{CallbackGate, CallbackRules, Catalog, State};
    use crate::tools::exec::eval::BATCH_CALLBACK;
    use crate::tools::integrations::ext::ToolCallback;
    with_agent("", |agent| {
        std::fs::write(agent.workspace.root().join("a.txt"), "alpha\n").unwrap();
        let mut backend = agent.backend.take().unwrap_or_else(|| {
            Backend::new(
                ProviderKind::Endpoint {
                    base_url: "http://127.0.0.1:1/v1".into(),
                    model: "test".into(),
                    api_key: None,
                },
                PathBuf::new(),
                5,
            )
        });
        let catalog = Catalog::default();
        let mut state = State::restore(&agent.session.control_state());
        let mut regenerations = 0;
        let mut gate = CallbackGate::new(CallbackRules {
            catalog: &catalog,
            state: &mut state,
            agent: "main",
            regenerations: &mut regenerations,
            max_regenerations: 3,
        });
        let mut callback = |agent: &mut Agent<'_>, name: &str, arguments: Value| {
            agent.code_callback(
                &mut backend,
                ToolCallback::Call {
                    name: name.into(),
                    arguments,
                },
                "cell",
                &mut NullUi,
                &mut gate,
                crate::agent::safety::approval::CallProvenance::Structured,
            )
        };
        let single = callback(agent, "functions.read_file", json!({"filePath":"a.txt"}));
        assert!(
            !single.is_error && single.text.contains("alpha"),
            "read alias must return the fixture content"
        );
        let mut batch = |agent: &mut Agent<'_>, calls: Value| {
            callback(agent, BATCH_CALLBACK, json!({"calls":calls}))
        };
        let reads = batch(
            agent,
            json!([
                {"name":"functions.read_file","arguments":{"filePath":"a.txt"}},
                {"name":"read","arguments":{"path":"missing.txt"}},
                {"name":"read","arguments":{"path":"a.txt"}},
            ]),
        );
        assert!(!reads.is_error, "{}", reads.text);
        let results = reads.data.unwrap();
        let results = results.as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0]["text"].as_str().unwrap().contains("alpha"));
        assert_eq!(results[1]["is_error"], true);
        assert_eq!(
            results[0]["text"], results[2]["text"],
            "entry order is kept"
        );

        // A non-read entry runs as an ordinary serial callback (NullUi denies); the reads on
        // each side of it still run as read groups, and every entry keeps its slot.
        agent.approval = Approval::Ask;
        let mixed = batch(
            agent,
            json!([
                {"name":"read","arguments":{"path":"a.txt"}},
                {"name":"read","arguments":{"path":"a.txt"}},
                {"name":"write_file","arguments":{"file_path":"b.txt","text":"x"}},
                {"name":"read","arguments":{"path":"missing.txt"}},
                {"name":"read","arguments":{"path":"a.txt"}},
            ]),
        );
        let results = mixed.data.unwrap();
        let results = results.as_array().unwrap();
        assert_eq!(results.len(), 5);
        for index in [0, 1, 4] {
            assert!(results[index]["text"].as_str().unwrap().contains("alpha"));
        }
        assert_eq!(results[2]["is_error"], true);
        assert_eq!(results[3]["is_error"], true);
        assert!(!agent.workspace.root().join("b.txt").exists());

        let nested = batch(
            agent,
            json!([{"name":BATCH_CALLBACK,"arguments":{"calls":[]}}]),
        );
        assert_eq!(nested.data.unwrap()[0]["is_error"], true);
        assert!(batch(agent, json!("not entries")).is_error);
        // The same executable name and arguments must reach deny rules in the shared-read lane.
        agent
            .always
            .push(crate::agent::safety::approval::PermissionRule {
                effect: crate::agent::safety::approval::Effect::Deny,
                tool: "read".into(),
                paths: vec!["a.txt".into()],
                command_prefix: vec![],
            });
        let alias = json!({"name":"read_file","arguments":{"filePath":"a.txt"}});
        let denied = batch(agent, json!([alias, alias]));
        for row in denied.data.unwrap().as_array().unwrap() {
            assert_eq!(row["is_error"], true);
            assert!(!row["text"].as_str().unwrap().contains("alpha"));
        }
        agent.always.clear();
        agent.hermetic = true;
        let strict = batch(
            agent,
            json!([
                {"name":"read","arguments":{"path":"a.txt"}},
                alias,
                {"name":"read","arguments":{"filePath":"a.txt"}}
            ]),
        );
        let rows = strict.data.unwrap();
        assert_eq!(rows[0]["is_error"], false);
        assert_eq!(rows[1]["is_error"], true);
        assert_eq!(rows[2]["is_error"], true);
    });
}
