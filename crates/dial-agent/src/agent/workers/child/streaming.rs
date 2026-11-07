//! Adapts the shared read-prefix observer to this child's live approval owner.
use crate::agent::run::streaming_dispatch::{StreamingDispatch, ToolStream};
use crate::llm::ToolCall;

pub(super) struct Authorized<'a> {
    pub stream: &'a mut StreamingDispatch,
    pub authorize: &'a mut dyn FnMut(&ToolCall) -> bool,
}
impl ToolStream for Authorized<'_> {
    fn arguments(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        self.stream
            .arguments_if(index, id, name, delta, self.authorize);
    }
    fn completed(&mut self, index: u64, id: &str, name: &str, args: &str) {
        self.stream
            .completed_if(index, id, name, args, self.authorize);
    }
    fn restart(&mut self) {
        self.stream.restart_reads();
    }
}
