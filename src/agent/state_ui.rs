//! One reducer in front of every surface. Callbacks are projections of the same mutable results.
use super::{Decision, Ui, todo::TodoList};
use crate::{
    llm::ModelProgress,
    runtime::{Body, Phase, Tree},
    tools::ToolOutput,
};
use serde_json::Value;
use std::{sync::atomic::AtomicBool, time::Duration};

pub(super) struct StateUi<'a> {
    inner: &'a mut dyn Ui,
    tree: Tree,
    parent: Option<String>,
    prose: Option<Body>,
    reasoning: Option<Body>,
    tools: Vec<(String, Body)>,
}
impl<'a> StateUi<'a> {
    pub fn new(inner: &'a mut dyn Ui, tree: Tree, parent: Option<String>) -> Self {
        inner.runtime(&tree);
        Self {
            inner,
            tree,
            parent,
            prose: None,
            reasoning: None,
            tools: Vec::new(),
        }
    }
    fn close_prose(&mut self) {
        for body in [self.prose.take(), self.reasoning.take()]
            .into_iter()
            .flatten()
        {
            let node = body.snapshot();
            body.settle(node.result.text, node.result.data, Phase::Done);
            self.inner.result_node(&body);
        }
    }
    pub fn finish(&mut self, interrupted: bool) {
        for body in [self.prose.take(), self.reasoning.take()]
            .into_iter()
            .flatten()
            .chain(self.tools.drain(..).map(|(_, body)| body))
        {
            let node = body.snapshot();
            body.settle(
                node.result.text,
                node.result.data,
                if interrupted || node.kind == "tool" {
                    Phase::Interrupted
                } else {
                    Phase::Done
                },
            );
            self.inner.result_node(&body);
        }
    }
}
impl Ui for StateUi<'_> {
    fn runtime_tree(&self) -> Option<Tree> {
        Some(self.tree.clone())
    }
    fn runtime(&mut self, tree: &Tree) {
        self.inner.runtime(tree)
    }
    fn result_node(&mut self, body: &Body) {
        self.inner.result_node(body)
    }
    fn text_delta(&mut self, text: &str) {
        if self.reasoning.is_some() {
            self.close_prose();
        }
        let body = self
            .prose
            .get_or_insert_with(|| self.tree.start("assistant", "answer", self.parent.clone()));
        body.append(text);
        self.inner.result_node(body);
        self.inner.text_delta(text);
    }
    fn reasoning_delta(&mut self, text: &str) {
        if self.prose.is_some() {
            self.close_prose();
        }
        let body = self.reasoning.get_or_insert_with(|| {
            self.tree
                .start("reasoning", "reasoning", self.parent.clone())
        });
        body.append(text);
        self.inner.result_node(body);
        self.inner.reasoning_delta(text);
    }
    fn tool_call(&mut self, name: &str, summary: &str) {
        self.close_prose();
        let body = self.tree.start("tool", name, self.parent.clone());
        self.inner.result_node(&body);
        self.tools.push((name.into(), body));
        self.inner.tool_call(name, summary);
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        if let Some((_, body)) = self.tools.iter().rev().find(|(n, _)| n == name) {
            body.append(text);
            self.inner.result_node(body);
        }
        self.inner.tool_output(name, text);
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        let body = self
            .tools
            .iter()
            .rposition(|(n, _)| n == name)
            .map(|i| self.tools.remove(i).1)
            .unwrap_or_else(|| self.tree.start("tool", name, self.parent.clone()));
        body.settle(
            output.text.clone(),
            output.data.clone(),
            if output.is_error {
                Phase::Failed
            } else {
                Phase::Done
            },
        );
        self.inner.result_node(&body);
        self.inner.tool_result(name, output);
    }
    fn notice(&mut self, v: &str) {
        self.inner.notice(v)
    }
    fn diagnostic(&mut self, v: &str) {
        self.inner.diagnostic(v)
    }
    fn background_notice(&mut self, v: &str) {
        self.inner.background_notice(v)
    }
    fn image(&mut self, v: &Value) {
        self.inner.image(v)
    }
    fn request_policy(&mut self, v: &Value) {
        self.inner.request_policy(v)
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
    fn approval_reason(&mut self) -> Option<String> {
        self.inner.approval_reason()
    }
    fn heartbeat(&mut self) {
        self.inner.heartbeat()
    }
    fn model_wait(&mut self, s: Duration) {
        self.inner.model_wait(s)
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
        assert_eq!(answer.result.phase, Phase::Done);
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
        assert_eq!(first_view.read(|n| n.result.phase), Phase::Done);
    }
}
