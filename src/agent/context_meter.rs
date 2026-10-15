//! Display-only context telemetry. Provider accounting remains separate from estimates.
use super::evidence;
use crate::{llm::Chat, storage::session::Session};

#[derive(Default)]
pub(super) struct ContextMeter {
    prompt: u64,
    request_estimate: u64,
    reported: bool,
    rebased: bool,
    anchor: usize,
}

/// Disjoint serialized byte categories, not tokenizer counts or billed usage.
#[derive(Default, serde::Serialize)]
pub(super) struct RequestOverhead {
    pub system_bytes: usize,
    pub tool_schema_bytes: usize,
    pub tool_result_bytes: usize,
    pub task_context_bytes: usize,
    pub conversation_bytes: usize,
    #[serde(skip)]
    pub economy_system_bytes: usize,
    #[serde(skip)]
    pub history_bytes: usize,
    #[serde(skip)]
    pub request_tail_bytes: usize,
}

impl RequestOverhead {
    #[cfg(test)]
    pub fn total(&self) -> usize {
        self.system_bytes
            + self.tool_schema_bytes
            + self.tool_result_bytes
            + self.task_context_bytes
            + self.conversation_bytes
    }
}

#[derive(Default)]
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_bytes(value: &serde_json::Value) -> usize {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).expect("JSON value serialization cannot fail");
    counter.0
}

/// Conservative wire cost of one attached image; `dial_images` stores only small JSON pointers.
const IMAGE_ESTIMATE_TOKENS: usize = 1_600;

/// Serialized bytes of a message as a provider receives it. Harness bookkeeping (`dial_*`) never
/// reaches the wire. Provider-native replay content is sent instead of, not beside, the portable
/// content/tool-call fields, so the larger of the two counts rather than their sum.
pub(super) fn wire_message_bytes(message: &serde_json::Value) -> usize {
    let Some(object) = message.as_object() else {
        return json_bytes(message);
    };
    let portable = object
        .iter()
        .filter(|(key, _)| !key.starts_with("dial_"))
        .map(|(key, value)| key.len() + 4 + json_bytes(value))
        .sum::<usize>()
        + 2;
    let native = object
        .get("dial_provider_content")
        .filter(|content| content.as_array().is_some_and(|items| !items.is_empty()))
        .map_or(0, |content| {
            json_bytes(content) + object.get("role").map_or(0, json_bytes) + 24
        });
    let images = object
        .get("dial_images")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    portable.max(native) + images * IMAGE_ESTIMATE_TOKENS * 4
}

pub(super) fn estimate_parts(messages: &[serde_json::Value], tools: &[serde_json::Value]) -> u64 {
    (messages.iter().map(wire_message_bytes).sum::<usize>()
        + tools.iter().map(json_bytes).sum::<usize>())
    .div_ceil(4) as u64
}

pub(super) fn overhead(request: &Chat) -> RequestOverhead {
    let mut out = RequestOverhead {
        tool_schema_bytes: request.tools.iter().map(json_bytes).sum(),
        ..Default::default()
    };
    for message in &request.messages {
        let bytes = json_bytes(message);
        if message["role"] == "system" {
            out.economy_system_bytes += bytes;
        } else if message["dial_transient"] == true {
            out.request_tail_bytes += bytes;
        } else {
            out.history_bytes += bytes;
        }
        let content = message["content"].as_str().unwrap_or_default();
        match message["role"].as_str() {
            Some("system" | "developer") => out.system_bytes += bytes,
            Some("tool") => out.tool_result_bytes += bytes,
            Some("user")
                if content.starts_with("[Task focus from dial")
                    || content
                        .trim_start()
                        .starts_with("[Current task evidence packet;") =>
            {
                out.task_context_bytes += bytes
            }
            _ => out.conversation_bytes += bytes,
        }
    }
    out
}

#[cfg(test)]
mod overhead_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn categories_are_disjoint_and_preserve_the_total_estimate() {
        let mut request = Chat {
            messages: vec![],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 8192,
            seed: None,
        };
        request.messages = vec![
            json!({"role":"system","content":"rules \"quoted\"\n\\escaped 雪","dial_transient":true}),
            json!({"role":"developer","content":"developer café"}),
            json!({"role":"tool","content":"evidence\t\u{0001}"}),
            json!({"role":"user","content":"[Current task evidence packet; harness context]\n{}"}),
            json!({"role":"user","content":"task 日本語"}),
            json!({"role":"user","content":"transient 尾","dial_transient":true}),
        ];
        request.tools = vec![json!({"type":"function","description":"\"雪\"\n\\"})];
        let cost = overhead(&request);
        let total: usize = request
            .messages
            .iter()
            .chain(request.tools.iter())
            .map(|v| v.to_string().len())
            .sum();
        assert_eq!(cost.total(), total);
        assert_eq!(
            cost.economy_system_bytes
                + cost.history_bytes
                + cost.request_tail_bytes
                + cost.tool_schema_bytes,
            total
        );
        assert_eq!(
            cost.economy_system_bytes,
            request.messages[0].to_string().len()
        );
        assert_eq!(
            cost.request_tail_bytes,
            request.messages[5].to_string().len()
        );
        assert_eq!(
            cost.history_bytes,
            request.messages[1..5]
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
        );
    }
}

impl ContextMeter {
    pub fn predict(&self, estimate: u64) -> u64 {
        if self.reported && self.request_estimate > 0 {
            if estimate >= self.request_estimate {
                self.prompt.saturating_add(estimate - self.request_estimate)
            } else {
                self.prompt.saturating_sub(self.request_estimate - estimate)
            }
        } else {
            estimate
        }
    }

    /// Move the display anchor after a successful compaction without losing the calibration
    /// between the last actual provider count and the serialized request that produced it.
    pub fn rebase(&mut self, estimate: u64, anchor: usize) {
        self.prompt = self.predict(estimate).max(estimate);
        self.request_estimate = estimate;
        self.anchor = anchor;
        self.rebased = true;
    }

    pub fn complete(&mut self, estimate: u64, predicted: u64, prompt: Option<u64>, anchor: usize) {
        self.prompt = prompt.unwrap_or(predicted);
        self.request_estimate = estimate;
        self.reported = prompt.is_some();
        self.anchor = anchor;
        self.rebased = false;
    }

    pub fn current(&self, session: &Session) -> (u64, bool) {
        let added = session.weigh_from(self.anchor, evidence::estimate) as u64;
        (
            self.prompt.saturating_add(added),
            !self.reported || self.rebased || session.message_count() > self.anchor,
        )
    }

    pub fn record_prompt(&mut self, tokens: u64, anchor: usize) {
        self.prompt = tokens;
        self.anchor = anchor;
        self.reported = true;
        self.rebased = false;
        // An externally supplied count has no matching serialized request to calibrate.
        self.request_estimate = 0;
    }
}

/// Accumulate bytes across fragments so one-character deltas still advance the meter.
pub(super) struct StreamingContext {
    prompt: u64,
    bytes: u64,
    reported_prompt: bool,
    output: Option<u64>,
}
impl StreamingContext {
    pub fn new(prompt: u64) -> Self {
        Self {
            prompt,
            bytes: 0,
            reported_prompt: false,
            output: None,
        }
    }
    pub fn telemetry(&mut self, progress: &crate::llm::ModelProgress) -> (u64, bool) {
        if let Some(prompt) = progress.prompt_tokens {
            self.prompt = prompt;
            self.reported_prompt = true;
        }
        if !progress.estimated
            && let Some(output) = progress.output_tokens
        {
            self.output = Some(output);
        }
        (self.current(), self.estimated())
    }
    fn current(&self) -> u64 {
        self.prompt
            .saturating_add(self.output.unwrap_or(self.bytes.div_ceil(4)))
    }
    pub fn estimated(&self) -> bool {
        !self.reported_prompt || self.output.is_none()
    }
    pub fn observe(&mut self, text: &str) -> u64 {
        self.bytes = self.bytes.saturating_add(text.len() as u64);
        self.current()
    }
    pub fn finish(&self, prompt: Option<u64>, output: Option<u64>) -> (u64, bool) {
        (
            prompt
                .unwrap_or(self.prompt)
                .saturating_add(output.or(self.output).unwrap_or(self.bytes.div_ceil(4))),
            (prompt.is_none() && !self.reported_prompt)
                || (output.is_none() && self.output.is_none()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_progress_calibrates_context_without_double_counting_text() {
        let mut context = StreamingContext::new(1000);
        let mut progress = crate::llm::ModelProgress::new(crate::llm::InferencePhase::Generating);
        progress.prompt_tokens = Some(64);
        progress.output_tokens = Some(12);
        assert_eq!(context.telemetry(&progress), (76, false));
        assert_eq!(context.observe("a visible answer"), 76);
        assert!(!context.estimated());
        assert_eq!(context.finish(Some(64), Some(14)), (78, false));
        assert_eq!(context.finish(None, None), (76, false));
    }
    #[test]
    fn fragments_accumulate_and_provider_usage_replaces_the_estimate() {
        let mut live = StreamingContext::new(100);
        for _ in 0..8 {
            live.observe("x");
        }
        assert_eq!(live.observe(""), 102);
        assert_eq!(live.finish(Some(80), Some(10)), (90, false));
        assert_eq!(live.finish(None, None), (102, true));
    }
    #[test]
    fn wire_estimate_ignores_bookkeeping_and_does_not_double_count_native_replay() {
        use serde_json::json;
        let plain = json!({"role":"assistant","content":"x".repeat(400)});
        let annotated = json!({"role":"assistant","content":"x".repeat(400),
            "dial_evidence":{"artifact":"artifact://a","observation":"y".repeat(4000)}});
        assert_eq!(wire_message_bytes(&annotated), wire_message_bytes(&plain));
        let native = json!({"role":"assistant","content":"x".repeat(400),
            "dial_provider_content":[{"type":"text","text":"x".repeat(400)}]});
        assert!(wire_message_bytes(&native) < 2 * wire_message_bytes(&plain));
        let image = json!({"role":"user","content":"look","dial_images":[{"path":"a.png"}]});
        assert!(estimate_parts(&[image], &[]) >= IMAGE_ESTIMATE_TOKENS as u64);
    }
    #[test]
    fn calibration_allows_context_to_shrink_after_projection() {
        let mut meter = ContextMeter::default();
        meter.complete(1000, 1000, Some(800), 0);
        assert_eq!(meter.predict(1200), 1000);
        assert_eq!(meter.predict(500), 300);
        assert_eq!(ContextMeter::default().predict(500), 500);
    }
}
