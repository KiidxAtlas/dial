//! Search-only schema projection. Never expose this text as the tool's description.
use serde_json::Value;

const MAX_CHARS: usize = 8192;
const MAX_FIELD_CHARS: usize = 1024;
const MAX_NODES: usize = 256;
const MAX_DEPTH: usize = 12;

pub(in crate::agent::run) fn text(schema: &Value) -> String {
    let mut index = Index {
        text: String::new(),
        chars: 0,
        nodes: 0,
    };
    index.push(schema["function"]["description"].as_str().unwrap_or(""));
    index.visit(&schema["function"]["parameters"], 0);
    index.text
}

struct Index {
    text: String,
    chars: usize,
    nodes: usize,
}
impl Index {
    fn push(&mut self, text: &str) {
        let remaining = MAX_CHARS.saturating_sub(self.chars);
        if remaining <= 1 || text.is_empty() {
            return;
        }
        self.text.push(' ');
        self.chars += 1;
        for ch in text.chars().take(MAX_FIELD_CHARS.min(remaining - 1)) {
            self.text.push(ch);
            self.chars += 1;
        }
    }

    fn visit(&mut self, schema: &Value, depth: usize) {
        if depth > MAX_DEPTH || self.nodes >= MAX_NODES || self.chars >= MAX_CHARS - 1 {
            return;
        }
        self.nodes += 1;
        if let Some(description) = schema["description"].as_str() {
            self.push(description);
        }
        if let Some(values) = schema["enum"].as_array() {
            for value in values.iter().take(MAX_NODES) {
                if let Some(value) = value.as_str() {
                    self.push(value);
                }
            }
        }
        if let Some(properties) = schema["properties"].as_object() {
            for (name, child) in properties.iter().take(MAX_NODES) {
                if self.nodes >= MAX_NODES {
                    break;
                }
                self.push(name);
                self.visit(child, depth + 1);
            }
        }
        // Only schema-bearing fields: defaults, examples, titles, IDs and arbitrary
        // extension metadata are not capability evidence. Do not resolve external refs.
        for key in ["items", "additionalProperties"] {
            if schema[key].is_object() {
                self.visit(&schema[key], depth + 1);
            }
        }
        for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(children) = schema[key].as_array() {
                for child in children.iter().take(MAX_NODES) {
                    self.visit(child, depth + 1);
                }
            }
        }
    }
}
