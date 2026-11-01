use super::*;
use crate::agent::run::streaming_dispatch::ToolStream;
use crate::agent::safety::rules::{Catalog, State};

#[derive(Default)]
struct Observed {
    arguments: usize,
    completions: usize,
}
impl ToolStream for Observed {
    fn arguments(&mut self, _: u64, _: &str, _: &str, _: &str) {
        self.arguments += 1;
    }
    fn completed(&mut self, _: u64, _: &str, _: &str, _: &str) {
        self.completions += 1;
    }
    fn restart(&mut self) {}
}

fn run(rule: &str, arguments: &str, prefix: Option<&str>, external_cancel: bool) -> Observed {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir_all(project.join(".dial/rules")).unwrap();
    std::fs::write(project.join(".dial/rules/guard.md"), rule).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    llm::chatgpt::test_account(&layout.user_dir, false);
    let overlay = toml::from_str(
        "model='account-model'\nprovider='chatgpt'\ncodex_websocket=false\n[retry]\nmax_attempts=1",
    )
    .unwrap();
    let config = Config::load(&layout, Some(overlay)).unwrap();
    let item = json!({"type":"function_call","name":"read","id":"item1","call_id":"call1","status":"completed","arguments":arguments});
    let mut events = Vec::new();
    if let Some(text) = prefix {
        events.push(json!({"type":"response.output_text.delta","delta":text}));
    }
    events.push(json!({"type":"response.output_item.added","output_index":0,"item":item}));
    events.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
    // A later delta exercises the same cancellation guard on the fallback path.
    if prefix.is_some() {
        events.push(json!({"type":"response.output_item.added","output_index":1,"item":item}));
        events.push(
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        );
    }
    events.push(
        json!({"type":"response.completed","response":{"status":"completed","output":[item]}}),
    );
    let wire: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let server = llm::fake::serve(vec![llm::fake::Script::Sse(wire)]);
    let mut backend = crate::agent::make_backend(&config).unwrap();
    backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
    let catalog = Catalog::discover(&layout, &project, &config.portable_rules);
    let state = State::default();
    let mut monitor = catalog.monitor(&project, &state, "main");
    let interrupt = Arc::new(AtomicBool::new(false));
    struct CancelUi(Arc<AtomicBool>, bool);
    impl Ui for CancelUi {
        fn text_delta(&mut self, _: &str) {
            if self.1 {
                self.0.store(true, Ordering::SeqCst);
            }
        }
    }
    let mut ui = CancelUi(interrupt.clone(), external_cancel);
    let mut observed = Observed::default();
    let _ = chat_attempt_with_ui(
        &mut backend,
        None,
        &Chat {
            messages: vec![json!({"role":"user","content":"read"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        },
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: None,
        },
        interrupt,
        ChatSinks {
            ui: &mut ui,
            on_idle: &mut |_| {},
        },
        ChatMonitors {
            rules: Some(&mut monitor),
            stream: Some(&mut observed),
        },
    );
    server.requests();
    observed
}

#[test]
fn completed_only_calls_pass_portable_rules_before_dispatch() {
    let observed = run(
        "---\ncondition: secret\nscope: tool:read\n---\nReview before reading",
        r#"{"path":"secret.txt"}"#,
        None,
        false,
    );
    assert_eq!(observed.completions, 0);
    let allowed = run(
        "---\ncondition: secret\nscope: tool:read\n---\nReview before reading",
        r#"{"path":"public.txt"}"#,
        None,
        false,
    );
    assert_eq!(
        allowed.completions, 1,
        "fixture must reach the completion handler"
    );
}

#[test]
fn malformed_completion_never_starts_speculative_read() {
    let observed = run(
        "---\ncondition: secret\nscope: tool:read\n---\nguard",
        "{",
        None,
        false,
    );
    assert_eq!(observed.completions, 0);
}

#[test]
fn late_tool_events_after_local_or_external_cancel_do_not_dispatch() {
    for external in [false, true] {
        let rule = if external {
            "---\ncondition: never-match\nscope: text\n---\nguard"
        } else {
            "---\ncondition: stop-now\nscope: text\n---\nguard"
        };
        let observed = run(rule, r#"{"path":"public.txt"}"#, Some("stop-now"), external);
        assert_eq!(observed.completions, 0);
        assert_eq!(observed.arguments, 0);
    }
}

#[test]
fn local_tool_turn_preserves_reasoning_and_prose_through_state_ui() {
    use crate::agent::ui::state_ui::StateUi;
    use crate::runtime::{Body, Phase, Tree, tool_output::ToolOutput};

    #[derive(Default)]
    struct View {
        reasoning: String,
        text: String,
        bodies: Vec<(String, String)>,
        events: Vec<&'static str>,
    }
    impl Ui for View {
        fn reasoning_delta(&mut self, text: &str) {
            self.reasoning.push_str(text);
            self.events.push("reasoning");
        }
        fn text_delta(&mut self, text: &str) {
            self.text.push_str(text);
            self.events.push("text");
        }
        fn result_node(&mut self, body: &Body) {
            let node = body.snapshot();
            self.bodies.push((node.kind, node.result.text));
        }
        fn tool_arguments_delta(&mut self, _: u64, _: &str, _: &str, _: &str) {
            assert_eq!(self.reasoning, "Inspect the notes first.");
            assert_eq!(self.text, "I will read the notes.");
            for (kind, text) in [
                ("reasoning", self.reasoning.as_str()),
                ("assistant", self.text.as_str()),
            ] {
                assert!(self.bodies.iter().any(|(k, t)| k == kind && t == text));
            }
            self.events.push("arguments");
        }
        fn tool_result(&mut self, _: &str, _: &ToolOutput) {
            self.events.push("result");
        }
    }

    // Both normal deltas and a completed message inside SSE are supported by the endpoint parser.
    for completed_message in [false, true] {
        let call = json!({"index":0,"id":"read-notes","type":"function",
            "function":{"name":"read","arguments":"{\"path\":\"notes.txt\"}"}});
        let events = if completed_message {
            vec![json!({"choices":[{"message":{
                "role":"assistant","reasoning_content":"Inspect the notes first.",
                "content":"I will read the notes.","tool_calls":[call]},
                "finish_reason":"tool_calls"}]})]
        } else {
            vec![
                json!({"choices":[{"delta":{"reasoning_content":"Inspect the notes first."}}]}),
                json!({"choices":[{"delta":{"content":"I will read the notes."}}]}),
                json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":"tool_calls"}]}),
            ]
        };
        let mut wire: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        wire.push_str("data: [DONE]\n\n");
        let server = llm::fake::serve(vec![llm::fake::Script::Sse(wire)]);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: format!("{}/v1", server.url),
                model: "gemma-4-test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"read the notes"})],
            tools: vec![json!({"type":"function","function":{"name":"read",
                "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}})],
            temperature: 0.0,
            max_tokens: 128,
            seed: None,
        };
        let tree = Tree::default();
        let mut view = View::default();
        {
            let mut ui = StateUi::new(&mut view, tree.clone(), None);
            let reply = chat_with_ui(
                &mut backend,
                None,
                &request,
                ChatOptions {
                    timeout: 5,
                    render_text: true,
                    context_tokens: None,
                    policy: None,
                    recovery: None,
                },
                Arc::new(AtomicBool::new(false)),
                &mut ui,
                &mut |_| {},
            )
            .unwrap();
            assert_eq!(reply.reasoning, "Inspect the notes first.");
            assert_eq!(reply.text, "I will read the notes.");
            assert_eq!(reply.tool_calls.len(), 1);
            let call = &reply.tool_calls[0];
            assert_eq!(call.arguments, json!({"path":"notes.txt"}));
            // Exercise the reducer's tool boundary; no real tool or filesystem access is needed.
            ui.tool_call_with_args(&call.id, &call.name, "notes.txt", &call.arguments);
            ui.tool_result(&call.name, &ToolOutput::ok("fixture notes".into()));
            ui.finish(false);
        }
        assert_eq!(view.events, ["reasoning", "text", "arguments", "result"]);
        for (kind, expected) in [
            ("reasoning", "Inspect the notes first."),
            ("assistant", "I will read the notes."),
        ] {
            let nodes = tree.snapshots();
            let node = nodes.iter().find(|node| node.kind == kind).unwrap();
            assert_eq!(node.result.text, expected);
            assert_eq!(node.result.phase, Phase::Completed);
        }
        assert_eq!(server.requests().len(), 1);
    }
}
