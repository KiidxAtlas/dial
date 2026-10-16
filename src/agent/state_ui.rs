//! One reducer in front of every surface. Callbacks are projections of the same mutable results.
use super::{Decision, Ui, todo::TodoList};
use crate::{
    llm::ModelProgress,
    runtime::{Body, Phase, Tree},
    tools::ToolOutput,
};
use serde_json::Value;
use std::{sync::atomic::AtomicBool, time::Duration};

struct PendingTool {
    name: String,
    call_id: Option<String>,
    body: Body,
    args: Value,
}

pub(super) struct StateUi<'a> {
    inner: &'a mut dyn Ui,
    tree: Tree,
    parent: Option<String>,
    prose: Option<Body>,
    reasoning: Option<Body>,
    inference: Option<Body>,
    tools: Vec<PendingTool>,
    registry: crate::tools::ext::Registry,
}
impl<'a> StateUi<'a> {
    /// Runtime observations reach every observer only after their journal write succeeded.
    fn publish(&mut self, event: impl FnOnce(&mut dyn Ui)) {
        if self.tree.check_persistence().is_ok() {
            event(self.inner);
        }
    }
    fn settle_inference(&mut self, phase: Phase) {
        if let Some(body) = self.inference.take() {
            let state = body.snapshot();
            let phase = if state.result.phase == Phase::Interrupted {
                Phase::Interrupted
            } else {
                phase
            };
            body.settle(state.result.text, state.result.data, phase);
            self.publish(|ui| ui.result_node(&body));
        }
    }
    #[cfg(test)]
    pub fn new(inner: &'a mut dyn Ui, tree: Tree, parent: Option<String>) -> Self {
        Self::with_registry(inner, tree, parent, crate::tools::ext::Registry::new())
    }
    pub fn with_registry(
        inner: &'a mut dyn Ui,
        tree: Tree,
        parent: Option<String>,
        registry: crate::tools::ext::Registry,
    ) -> Self {
        inner.runtime(&tree);
        Self {
            inner,
            tree,
            parent,
            inference: None,
            prose: None,
            reasoning: None,
            tools: Vec::new(),
            registry,
        }
    }
    fn close_prose(&mut self) {
        for body in [self.prose.take(), self.reasoning.take()]
            .into_iter()
            .flatten()
        {
            let node = body.snapshot();
            body.settle(node.result.text, node.result.data, Phase::Completed);
            self.publish(|ui| ui.result_node(&body));
        }
    }
    fn start_tool(&mut self, name: &str, call_id: Option<&str>) {
        self.settle_inference(Phase::Completed);
        self.close_prose();
        let body = self.tree.start("tool", name, self.parent.clone());
        if let Some(id) = call_id {
            body.correlate(id);
        }
        self.publish(|ui| ui.result_node(&body));
        self.tools.push(PendingTool {
            name: name.into(),
            call_id: call_id.map(str::to_owned),
            body,
            args: Value::Null,
        });
    }
    fn settle_tool(&mut self, name: &str, call_id: Option<&str>, output: &ToolOutput) {
        let position = match call_id {
            Some(id) => self
                .tools
                .iter()
                .rposition(|tool| tool.call_id.as_deref() == Some(id)),
            None => self.tools.iter().rposition(|tool| tool.name == name),
        };
        let (body, args) = position
            .map(|i| {
                let tool = self.tools.remove(i);
                (tool.body, tool.args)
            })
            .unwrap_or_else(|| {
                (
                    self.tree.start("tool", name, self.parent.clone()),
                    Value::Null,
                )
            });
        let mut view = if args.is_null() {
            crate::tools::presentation::render_result(name, &args, output)
        } else {
            self.registry.render_result(name, &args, output)
        };
        if args.is_null()
            && let Some(call_view) = body.read(|node| node.presentation.clone())
        {
            view.target = call_view.target;
        }
        body.present(view.clone());
        body.settle(
            output.text.clone(),
            output.data.clone(),
            self.registry.outcome(name, &args, output),
        );
        let mut execution = output.execution.clone();
        execution.status = Some(crate::tools::ToolStatus::from_output(output));
        body.execution(execution);
        self.publish(|ui| {
            if let Some(id) = call_id {
                ui.tool_presentation(id, &view);
            }
            ui.result_node(&body);
        });
    }
    fn discard_inference(&mut self) {
        for body in [self.prose.take(), self.reasoning.take()]
            .into_iter()
            .flatten()
        {
            let node = body.snapshot();
            body.settle(node.result.text, node.result.data, Phase::Interrupted);
            self.publish(|ui| ui.result_node(&body));
        }
        self.settle_inference(Phase::Interrupted);
    }
    pub fn finish(&mut self, interrupted: bool) {
        self.settle_inference(if interrupted {
            Phase::Interrupted
        } else {
            Phase::Completed
        });
        for body in [self.prose.take(), self.reasoning.take()]
            .into_iter()
            .flatten()
            .chain(self.tools.drain(..).map(|tool| tool.body))
            .collect::<Vec<_>>()
        {
            let node = body.snapshot();
            body.settle(
                node.result.text,
                node.result.data,
                if interrupted || node.kind == "tool" {
                    Phase::Interrupted
                } else {
                    Phase::Completed
                },
            );
            self.publish(|ui| ui.result_node(&body));
        }
    }
}
impl Ui for StateUi<'_> {
    fn execution_error(&self) -> Option<String> {
        self.tree
            .check_persistence()
            .err()
            .or_else(|| self.inner.execution_error())
    }
    fn renders_workers(&self) -> bool {
        self.inner.renders_workers()
    }
    fn runtime_tree(&self) -> Option<Tree> {
        Some(self.tree.clone())
    }
    fn runtime(&mut self, tree: &Tree) {
        self.inner.runtime(tree)
    }
    fn result_node(&mut self, body: &Body) {
        self.publish(|ui| ui.result_node(body))
    }
    fn text_delta(&mut self, text: &str) {
        if self.reasoning.is_some() {
            self.close_prose();
        }
        let body = self
            .prose
            .get_or_insert_with(|| self.tree.start("assistant", "answer", self.parent.clone()))
            .clone();
        body.append(text);
        self.publish(|ui| {
            ui.result_node(&body);
            ui.text_delta(text);
        });
    }
    fn reasoning_delta(&mut self, text: &str) {
        if self.prose.is_some() {
            self.close_prose();
        }
        let body = self
            .reasoning
            .get_or_insert_with(|| {
                self.tree
                    .start("reasoning", "reasoning", self.parent.clone())
            })
            .clone();
        body.append(text);
        self.publish(|ui| {
            ui.result_node(&body);
            ui.reasoning_delta(text);
        });
    }
    fn completion(&mut self, summary: &Value) {
        let phase = Phase::from_status(summary["outcome"].as_str().unwrap_or("failed"));
        let body = self
            .tree
            .start("completion", "Turn outcome", self.parent.clone());
        body.settle(String::new(), Some(summary.clone()), phase);
        self.publish(|ui| {
            ui.result_node(&body);
            ui.completion(summary);
        });
    }
    fn tool_call(&mut self, name: &str, summary: &str) {
        self.start_tool(name, None);
        if let Some(tool) = self.tools.last() {
            tool.body
                .present(super::legacy_tool_presentation(name, summary, ""));
        }
        self.publish(|ui| ui.tool_call(name, summary));
    }
    fn tool_call_with_id(&mut self, call_id: &str, name: &str, summary: &str) {
        self.start_tool(name, Some(call_id));
        if let Some(tool) = self.tools.last() {
            tool.body
                .present(super::legacy_tool_presentation(name, summary, ""));
        }
        self.publish(|ui| ui.tool_call_with_id(call_id, name, summary));
    }
    fn tool_call_with_args(&mut self, call_id: &str, name: &str, summary: &str, args: &Value) {
        self.start_tool(name, Some(call_id));
        let view = self.registry.render_call(name, args);
        if let Some(tool) = self.tools.last_mut() {
            tool.args = args.clone();
            tool.body.present(view.clone());
            let body = tool.body.clone();
            self.publish(|ui| ui.result_node(&body));
        }
        self.publish(|ui| {
            ui.tool_call_with_args(call_id, name, summary, args);
            ui.tool_presentation(call_id, &view);
        });
    }
    fn tool_presentation(
        &mut self,
        call_id: &str,
        view: &crate::tools::presentation::Presentation,
    ) {
        if let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.call_id.as_deref() == Some(call_id))
        {
            tool.body.present(view.clone());
        }
        self.publish(|ui| ui.tool_presentation(call_id, view));
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        if let Some(tool) = self.tools.iter().rev().find(|tool| tool.name == name) {
            tool.body.append(text);
            let body = tool.body.clone();
            self.publish(|ui| ui.result_node(&body));
        }
        self.publish(|ui| ui.tool_output(name, text));
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        self.settle_tool(name, None, output);
        self.publish(|ui| ui.tool_result(name, output));
    }
    fn tool_result_with_id(&mut self, call_id: &str, name: &str, output: &ToolOutput) {
        self.settle_tool(name, Some(call_id), output);
        self.publish(|ui| ui.tool_result_with_id(call_id, name, output));
    }
    fn notice(&mut self, v: &str) {
        self.inner.notice(v)
    }
    fn event(&mut self, event: &super::ui_event::UiEvent) {
        self.inner.event(event)
    }
    fn changed_files(&mut self, paths: &[String]) {
        self.inner.changed_files(paths)
    }
    fn diagnostic(&mut self, v: &str) {
        self.inner.diagnostic(v)
    }
    fn background_notice(&mut self, v: &str) {
        self.inner.background_notice(v)
    }
    fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        self.inner.background_output(name, text, replace)
    }
    fn image(&mut self, v: &Value) {
        self.inner.image(v)
    }
    fn request_policy(&mut self, v: &Value) {
        self.inner.request_policy(v)
    }
    fn efficiency(&mut self, report: &Value) {
        self.inner.efficiency(report)
    }
    fn laya_activity(&mut self, status: &str, message: &str) {
        self.inner.laya_activity(status, message)
    }
    fn usage(&mut self, p: Option<u64>, o: Option<u64>, s: f64) {
        self.inner.usage(p, o, s)
    }
    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.inner.context_usage(tokens, estimated)
    }
    fn model_progress(&mut self, p: &ModelProgress) {
        self.inner.model_progress(p)
    }
    fn tool_arguments_delta(&mut self, i: u64, id: &str, n: &str, d: &str) {
        self.inner.tool_arguments_delta(i, id, n, d)
    }
    fn cached_usage(&mut self, r: Option<u64>, w: Option<u64>) {
        self.inner.cached_usage(r, w)
    }
    fn extension_panel(&mut self, t: &str, m: &str) {
        self.inner.extension_panel(t, m)
    }
    fn role(&mut self, r: &str, m: &str) {
        self.close_prose();
        self.inner.role(r, m)
    }
    fn approve(&mut self, t: &str, s: &str) -> Decision {
        self.inner.approve(t, s)
    }
    fn approve_with_cancel(&mut self, t: &str, s: &str, c: &AtomicBool) -> Decision {
        self.inner.approve_with_cancel(t, s, c)
    }
    fn ask_user(&mut self, title: &str, question: &str, options: &[String]) -> Option<String> {
        self.inner.ask_user(title, question, options)
    }
    fn ask_questions(
        &mut self,
        request: &super::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> super::questions::QuestionOutcome {
        self.inner.ask_questions(request, cancel)
    }
    fn inference_discarded(&mut self, reason: &str) {
        self.discard_inference();
        self.inner.inference_discarded(reason);
    }
    fn interactive_input(&self) -> bool {
        self.inner.interactive_input()
    }

    fn approval_reason(&mut self) -> Option<String> {
        self.inner.approval_reason()
    }
    fn heartbeat(&mut self) {
        self.inner.heartbeat()
    }
    fn model_wait(&mut self, s: Duration) {
        self.inner.model_wait(s)
    }
    fn request_phase(&mut self, phase: super::RequestPhase) {
        use super::RequestPhase;
        if phase == RequestPhase::Waiting {
            // Each model request answers on its own: the previous reply is a finished message,
            // so the next one must not continue it mid-sentence.
            self.close_prose();
            self.settle_inference(Phase::Completed);
            self.inference = Some(self.tree.start(
                "inference",
                "Model request",
                self.parent.clone(),
            ));
        }
        if let Some(body) = &self.inference {
            let state = if phase == RequestPhase::Cancelling {
                Phase::Interrupted
            } else {
                Phase::Running
            };
            body.settle(
                phase.label().to_string(),
                Some(serde_json::json!({"phase":phase.label(),"scope":"inference attempt"})),
                state,
            );
            let body = body.clone();
            self.publish(|ui| ui.result_node(&body));
        }
        self.inner.request_phase(phase)
    }
    fn take_steering(&mut self) -> Vec<String> {
        self.inner.take_steering()
    }
    fn maintenance_ready(&mut self) -> bool {
        self.inner.maintenance_ready()
    }
    fn tool_timing(&mut self, n: &str, s: f64) {
        self.inner.tool_timing(n, s)
    }
    fn todo(&mut self, t: &TodoList) {
        self.inner.todo(t)
    }
}

#[cfg(test)]
mod tests {
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
            ext::{Registry, Tool, ToolContext},
            presentation::{BodyStyle, Presentation},
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
        ui.request_phase(super::super::RequestPhase::Waiting);
        ui.request_phase(super::super::RequestPhase::Cancelling);
        ui.request_phase(super::super::RequestPhase::Waiting);
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
}
