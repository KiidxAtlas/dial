//! Bounded snapshots of the requests that actually produced learnable assistant targets.
use serde_json::{Value, json};

const MAX_EXAMPLES: usize = 8;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(in crate::agent) struct Capture {
    examples: Vec<Value>,
    bytes: usize,
}

impl Capture {
    pub fn clear(&mut self) {
        self.examples.clear();
        self.bytes = 0;
    }

    pub fn record(&mut self, context: &[Value], tools: &[Value], target: Value) {
        let mut messages: Vec<_> = context
            .iter()
            .map(crate::llm::messages::canonical)
            .collect();
        messages.push(crate::llm::messages::canonical(&target));
        let example = json!({"messages":messages,"tools":tools});
        let size = example.to_string().len();
        if size > MAX_BYTES {
            return;
        }
        while self.examples.len() >= MAX_EXAMPLES || self.bytes + size > MAX_BYTES {
            self.bytes -= self.examples.remove(0).to_string().len();
        }
        self.bytes += size;
        self.examples.push(example);
    }

    pub fn payload(&self) -> Value {
        json!({"examples":self.examples})
    }
}

#[cfg(test)]
mod tests;
