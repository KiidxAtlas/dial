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
mod overhead_tests;

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
mod tests;
