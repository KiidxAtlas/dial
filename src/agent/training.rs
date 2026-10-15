//! Bounded snapshots of the requests that actually produced learnable assistant targets.
use serde_json::{Value, json};

const MAX_EXAMPLES: usize = 8;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct Capture {
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
mod tests {
    use super::*;

    #[test]
    fn snapshots_retain_actual_context_and_only_the_new_target() {
        let mut capture = Capture::default();
        let mut request = vec![
            json!({"role":"system","content":"working state and recalled memory"}),
            json!({"role":"user","content":"original constraints"}),
            json!({"role":"assistant","content":"previous answer"}),
            json!({"role":"user","content":"continue"}),
        ];
        capture.record(
            &request,
            &[],
            json!({"role":"assistant","content":"new answer"}),
        );
        request[0]["content"] = json!("changed after inference");
        let payload = capture.payload();
        let messages = payload["examples"][0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["content"], "working state and recalled memory");
        assert_eq!(messages[1]["content"], "original constraints");
        assert_eq!(messages[4]["content"], "new answer");
        capture.clear();
        assert_eq!(capture.payload()["examples"], json!([]));
    }

    #[test]
    fn bounded_capture_keeps_the_final_target_and_its_schema() {
        let mut capture = Capture::default();
        for i in 0..20 {
            capture.record(
                &[json!({"role":"user","content":"task"})],
                &[json!({"schema":i})],
                json!({"role":"assistant","content":i.to_string()}),
            );
        }
        assert_eq!(capture.examples.len(), MAX_EXAMPLES);
        assert_eq!(capture.examples.last().unwrap()["tools"][0]["schema"], 19);
        assert_eq!(
            capture.examples.last().unwrap()["messages"][1]["content"],
            "19"
        );
        capture.record(
            &[json!({"role":"user","content":"x".repeat(MAX_BYTES)})],
            &[],
            json!({"role":"assistant","content":"oversized"}),
        );
        assert_eq!(capture.examples.len(), MAX_EXAMPLES);
        assert!(capture.bytes <= MAX_BYTES);
    }
}
