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
    let data = output.data.unwrap();
    assert_eq!(data[0]["changed_paths"][0], "output.bin");
    assert_eq!(data[1]["status"], "blocked");
    assert_eq!(data[1]["effects"], "none");
    assert_eq!(data[2]["effects"], "unknown");
    assert_eq!(data[2]["index"], 2);
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
        agent.always.push(PermissionRule {
            effect: Effect::Deny,
            tool: "read".into(),
            command_prefix: vec![],
            paths: vec![],
        });
        assert!(matches!(
            agent.authorize_call_from(&call, CallProvenance::RecoveredText, &mut view),
            Err(Denial::Rule)
        ));
        agent.always.clear();
        agent.interrupt.store(true, Ordering::SeqCst);
        assert!(matches!(
            agent.authorize_call_from(&call, CallProvenance::RecoveredText, &mut view),
            Err(Denial::User)
        ));
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
        let mut batch = |agent: &mut Agent<'_>, calls: Value| {
            agent.code_callback(
                &mut backend,
                ToolCallback::Call {
                    name: BATCH_CALLBACK.into(),
                    arguments: json!({ "calls": calls }),
                },
                "cell",
                &mut NullUi,
                &mut gate,
                crate::agent::safety::approval::CallProvenance::Structured,
            )
        };
        let reads = batch(
            agent,
            json!([
                {"name":"read","arguments":{"path":"a.txt"}},
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

        // A non-read entry makes every entry an ordinary serial callback (NullUi denies).
        agent.approval = Approval::Ask;
        let mixed = batch(
            agent,
            json!([
                {"name":"read","arguments":{"path":"a.txt"}},
                {"name":"write","arguments":{"path":"b.txt","content":"x"}},
            ]),
        );
        let results = mixed.data.unwrap();
        assert!(results[0]["text"].as_str().unwrap().contains("alpha"));
        assert_eq!(results[1]["is_error"], true);
        assert!(!agent.workspace.root().join("b.txt").exists());

        let nested = batch(
            agent,
            json!([{"name":BATCH_CALLBACK,"arguments":{"calls":[]}}]),
        );
        assert_eq!(nested.data.unwrap()[0]["is_error"], true);
        assert!(batch(agent, json!("not entries")).is_error);
    });
}
