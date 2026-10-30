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
