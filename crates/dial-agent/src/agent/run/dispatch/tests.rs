use super::*;

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
