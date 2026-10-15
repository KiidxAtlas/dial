//! Explicit mode stages. Role adapters invoke these stages; mode implementations own policy.
use super::super::{Agent, Ui};
use crate::{
    llm::{ToolCall, provider::Backend},
    tools::ToolOutput,
};
use serde_json::Value;

pub(in crate::agent) trait Lifecycle {
    fn before_request(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> anyhow::Result<()>;
    fn before_yield(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> anyhow::Result<bool>;
    fn dispatched(&mut self, call: &ToolCall, output: &ToolOutput);
    fn settled(&mut self, call: &ToolCall, output: &ToolOutput);
}

impl Lifecycle for Agent<'_> {
    fn before_request(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> anyhow::Result<()> {
        self.prewalk_step_start(backend, names, nudges, ui)
    }
    fn before_yield(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> anyhow::Result<bool> {
        self.prewalk_turn_end(backend, names, nudges, ui)
    }
    fn dispatched(&mut self, call: &ToolCall, output: &ToolOutput) {
        self.modes.goal.record(&call.name, &call.arguments, output);
    }
    fn settled(&mut self, call: &ToolCall, output: &ToolOutput) {
        self.prewalk_observe(&call.name, !output.is_error);
    }
}
