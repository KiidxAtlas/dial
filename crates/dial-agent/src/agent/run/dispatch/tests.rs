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
    for tool in ["visual", "video", "evidence", "read"] {
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
