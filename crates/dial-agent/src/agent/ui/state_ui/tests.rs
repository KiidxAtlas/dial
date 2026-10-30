use super::*;
#[test]
fn journal_failure_stops_all_runtime_publication_and_preserves_saved_prose() {
    use std::{cell::Cell, rc::Rc};
    struct Observer(Rc<Cell<usize>>);
    impl Ui for Observer {
        fn result_node(&mut self, _: &Body) {
            self.0.set(self.0.get() + 1);
        }
        fn text_delta(&mut self, _: &str) {
            self.0.set(self.0.get() + 1);
        }
        fn reasoning_delta(&mut self, _: &str) {
            self.0.set(self.0.get() + 1);
        }
        fn tool_call(&mut self, _: &str, _: &str) {
            self.0.set(self.0.get() + 1);
        }
        fn tool_output(&mut self, _: &str, _: &str) {
            self.0.set(self.0.get() + 1);
        }
        fn tool_result(&mut self, _: &str, _: &ToolOutput) {
            self.0.set(self.0.get() + 1);
        }
        fn completion(&mut self, _: &Value) {
            self.0.set(self.0.get() + 1);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("session.jsonl");
    let journal = crate::runtime::stream_journal::Journal::open(&session).unwrap();
    let tree = Tree::default();
    tree.journal(journal.clone());
    let count = Rc::new(Cell::new(0));
    let mut observer = Observer(count.clone());
    let mut ui = StateUi::new(&mut observer, tree.clone(), None);
    ui.text_delta("durable prefix");
    assert_eq!(count.get(), 2);
    let saved = tree.snapshots();
    journal.fail_writes_for_test(
        std::fs::File::open(crate::runtime::stream_journal::path(&session)).unwrap(),
    );
    ui.text_delta("not durable");
    ui.reasoning_delta("not durable");
    ui.tool_call("bash", "call");
    ui.tool_output("bash", "not durable");
    ui.tool_result("bash", &ToolOutput::ok("result".into()));
    ui.completion(&serde_json::json!({"outcome":"completed"}));
    ui.finish(false);
    assert!(ui.execution_error().is_some());
    assert_eq!(
        tree.snapshots(),
        saved,
        "polling observers also see only durable state"
    );
    assert_eq!(
        count.get(),
        2,
        "no observer sees an observation whose write failed"
    );
    let recovered = crate::runtime::stream_journal::recover(&session).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].result.text, "durable prefix");
}
#[test]
fn coherent_runtime_extensions_own_call_and_result_presentation() {
    use crate::tools::{
        integrations::ext::{Registry, Tool, ToolContext},
        surface::presentation::{BodyStyle, Presentation},
    };
    struct Extension;
    impl Tool for Extension {
        fn name(&self) -> &str {
            "custom_example"
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type":"function","function":{"name":"custom_example","description":"fake","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::err("unused")
        }
        fn render_call(&self, args: &Value) -> Presentation {
            Presentation {
                label: "Inspect".into(),
                target: args["owner"].as_str().unwrap().into(),
                body: BodyStyle::Quiet,
                ..Default::default()
            }
        }
        fn render_result(&self, args: &Value, _: &ToolOutput) -> Presentation {
            let mut view = self.render_call(args);
            view.facts.push("3 findings".into());
            view.preview = Some("compact answer".into());
            view
        }
    }
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::new(Extension));
    let tree = Tree::default();
    let mut sink = View::default();
    let mut ui = StateUi::with_registry(&mut sink, tree.clone(), None, registry);
    ui.tool_call_with_args(
        "call-custom",
        "custom_example",
        "opaque summary",
        &serde_json::json!({"owner":"parser"}),
    );
    ui.tool_result_with_id(
        "call-custom",
        "custom_example",
        &ToolOutput {
            execution: Default::default(),
            text: "full evidence".into(),
            data: None,
            is_error: false,
        },
    );
    let node = tree
        .snapshots()
        .into_iter()
        .find(|node| node.kind == "tool")
        .unwrap();
    assert_eq!(node.call_id.as_deref(), Some("call-custom"));
    assert_eq!(node.result.phase, Phase::Completed);
    assert_eq!(node.result.text, "full evidence");
    let view = node.presentation.as_ref().unwrap();
    assert_eq!(
        (view.label.as_str(), view.target.as_str()),
        ("Inspect", "parser")
    );
    assert_eq!(view.facts, ["3 findings"]);
    let restored = Tree::default();
    restored.restore(serde_json::from_value(serde_json::to_value(&node).unwrap()).unwrap());
    assert_eq!(restored.snapshots()[0], node);
}

#[test]
fn coherent_runtime_completion_keeps_every_terminal_phase() {
    let tree = Tree::default();
    let mut sink = View::default();
    let mut ui = StateUi::new(&mut sink, tree.clone(), None);
    for phase in [
        Phase::Completed,
        Phase::Blocked,
        Phase::NeedsInput,
        Phase::Incomplete,
        Phase::Failed,
        Phase::Interrupted,
    ] {
        ui.completion(&serde_json::json!({"outcome":phase}));
        assert_eq!(tree.snapshots().last().unwrap().result.phase, phase);
    }
}

#[test]
fn coherent_runtime_worker_incompletion_is_not_a_tool_failure() {
    let tree = Tree::default();
    let mut sink = View::default();
    let mut ui = StateUi::new(&mut sink, tree.clone(), None);
    ui.tool_call_with_args(
        "task-1",
        "task",
        "Worker",
        &serde_json::json!({"prompt":"inspect"}),
    );
    ui.tool_result_with_id(
        "task-1",
        "task",
        &ToolOutput {
            execution: Default::default(),
            text: "Worker could not finish".into(),
            is_error: true,
            data: Some(serde_json::json!({"jobs":[{"status":"incomplete"}]})),
        },
    );
    assert_eq!(tree.snapshots()[0].result.phase, Phase::Incomplete);
}
#[derive(Default)]
struct View {
    nodes: Vec<Body>,
}
impl Ui for View {
    fn result_node(&mut self, body: &Body) {
        self.nodes.push(body.clone());
    }
    fn approve(&mut self, _: &str, _: &str) -> Decision {
        Decision::Allow
    }
}
#[test]
fn alternating_streams_settle_before_switching_and_interrupts_leave_workers_running() {
    let tree = Tree::default();
    tree.anchor(Some("user-message".into()));
    let worker = tree.start("worker", "independent", None);
    let mut view = View::default();
    {
        let mut ui = StateUi::new(&mut view, tree.clone(), None);
        ui.reasoning_delta("inspect");
        ui.text_delta("first");
        ui.text_delta(" second");
        ui.reasoning_delta("check");
        ui.tool_call("browser", "screenshot");
        ui.tool_output("browser", "preview");
        ui.tool_result(
            "browser",
            &ToolOutput::structured(serde_json::json!({"image":{"width":1200}})),
        );
        assert!(matches!(ui.approve("write", "test"), Decision::Allow));
        ui.tool_call("bash", "pending");
        ui.finish(true);
    }
    let nodes = tree.snapshots();
    let answer = nodes.iter().find(|n| n.kind == "assistant").unwrap();
    assert_eq!(answer.result.text, "first second");
    assert_eq!(answer.parent.as_deref(), Some("user-message"));
    assert_eq!(answer.result.phase, Phase::Completed);
    let browser = nodes.iter().find(|n| n.label == "browser").unwrap();
    assert_eq!(
        browser.result.data.as_ref().unwrap()["image"]["width"],
        1200
    );
    assert_eq!(
        nodes
            .iter()
            .find(|n| n.label == "bash")
            .unwrap()
            .result
            .phase,
        Phase::Interrupted
    );
    assert_eq!(worker.read(|n| n.result.phase), Phase::Running);
    let first_view = view
        .nodes
        .iter()
        .find(|b| b.read(|n| n.label == "browser"))
        .unwrap();
    assert_eq!(first_view.read(|n| n.result.phase), Phase::Completed);
}
#[test]
fn wrapped_input_preserves_capability_response_and_cancellation() {
    struct Input {
        interactive: bool,
        response: Option<String>,
    }
    impl Ui for Input {
        fn interactive_input(&self) -> bool {
            self.interactive
        }
        fn ask_user(&mut self, _: &str, _: &str, _: &[String]) -> Option<String> {
            self.response.take()
        }
    }
    for interactive in [false, true] {
        let mut input = Input {
            interactive,
            response: Some("direction".into()),
        };
        let mut wrapped = StateUi::new(&mut input, Tree::default(), None);
        assert_eq!(wrapped.interactive_input(), interactive);
        assert_eq!(
            wrapped.ask_user("title", "question", &[]).as_deref(),
            Some("direction")
        );
        assert_eq!(wrapped.ask_user("title", "question", &[]), None);
        assert_eq!(wrapped.interactive_input(), interactive);
    }
}

#[test]
fn request_cancellation_and_completion_have_one_shared_terminal_state() {
    struct Sink;
    impl Ui for Sink {}
    let tree = Tree::default();
    let mut sink = Sink;
    let mut ui = StateUi::new(&mut sink, tree.clone(), None);
    ui.request_phase(crate::agent::RequestPhase::Waiting);
    ui.request_phase(crate::agent::RequestPhase::Cancelling);
    ui.request_phase(crate::agent::RequestPhase::Waiting);
    ui.completion(&serde_json::json!({"outcome":"incomplete","open_tasks":1}));
    ui.finish(true);
    let nodes = tree.snapshots();
    assert!(
        nodes
            .iter()
            .filter(|node| node.kind == "inference")
            .all(|node| node.result.phase == Phase::Interrupted)
    );
    let completion = nodes.iter().find(|node| node.kind == "completion").unwrap();
    assert_eq!(completion.result.phase, Phase::Incomplete);
    assert_eq!(completion.result.data.as_ref().unwrap()["open_tasks"], 1);
}
#[test]
fn retry_backoff_stays_in_one_request_and_timings_survive_settlement() {
    use crate::agent::RequestPhase;
    struct Sink;
    impl Ui for Sink {}
    let tree = Tree::default();
    let mut sink = Sink;
    let mut ui = StateUi::new(&mut sink, tree.clone(), None);
    ui.request_phase(RequestPhase::Waiting);
    ui.request_phase(RequestPhase::Backoff);
    ui.request_phase(RequestPhase::Retrying);
    ui.request_phase(RequestPhase::Generating);
    ui.request_finished(true);
    ui.request_finished(true); // repeated settlement must not double count
    assert_eq!(ui.request_latency()["requests"], 1);
    assert_eq!(ui.request_latency()["retry_backoffs"], 1);
    ui.finish(false);
    let nodes = tree.snapshots();
    let requests: Vec<_> = nodes
        .iter()
        .filter(|node| node.kind == "inference")
        .collect();
    assert_eq!(requests.len(), 1);
    let data = requests[0].result.data.as_ref().unwrap();
    assert_eq!(data["latency"]["requests"], 1);
    assert_eq!(data["latency"]["failed_requests"], 0);
    assert!(data["latency"]["first_generation_ms"].is_number());
}

#[test]
fn discarded_request_retains_failed_latency_without_reopening_its_node() {
    use crate::agent::RequestPhase;
    struct Sink;
    impl Ui for Sink {}
    let tree = Tree::default();
    let mut sink = Sink;
    let mut ui = StateUi::new(&mut sink, tree.clone(), None);
    ui.request_phase(RequestPhase::Waiting);
    ui.request_phase(RequestPhase::Cancelling);
    ui.inference_discarded("silent request");
    ui.request_finished(false);
    let nodes = tree.snapshots();
    let request = nodes.iter().find(|node| node.kind == "inference").unwrap();
    assert_eq!(request.result.phase, Phase::Interrupted);
    assert_eq!(
        request.result.data.as_ref().unwrap()["latency"]["failed_requests"],
        1
    );
    assert_eq!(ui.request_latency()["failed_requests"], 1);
}
