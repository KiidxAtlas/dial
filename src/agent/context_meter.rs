//! Display-only context telemetry. Provider accounting remains separate from estimates.
use super::evidence;
use crate::{llm::Chat, storage::session::Session};

#[derive(Default)]
pub(super) struct ContextMeter {
    prompt: u64,
    request_estimate: u64,
    reported: bool,
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
}

impl RequestOverhead {
    pub fn total(&self) -> usize {
        self.system_bytes
            + self.tool_schema_bytes
            + self.tool_result_bytes
            + self.task_context_bytes
            + self.conversation_bytes
    }
}

pub(super) fn overhead(request: &Chat) -> RequestOverhead {
    let mut out = RequestOverhead::default();
    out.tool_schema_bytes = request.tools.iter().map(|t| t.to_string().len()).sum();
    for message in &request.messages {
        let bytes = message.to_string().len();
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

pub(super) fn estimate_request(request: &Chat) -> u64 {
    overhead(request).total().div_ceil(4) as u64
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
            json!({"role":"system","content":"rules"}),
            json!({"role":"tool","content":"evidence"}),
            json!({"role":"user","content":"[Current task evidence packet; harness context]\n{}"}),
            json!({"role":"user","content":"task"}),
        ];
        request.tools = vec![json!({"type":"function"})];
        let cost = overhead(&request);
        assert!(
            cost.system_bytes > 0
                && cost.tool_schema_bytes > 0
                && cost.tool_result_bytes > 0
                && cost.task_context_bytes > 0
                && cost.conversation_bytes > 0
        );
        let total: usize = request
            .messages
            .iter()
            .chain(request.tools.iter())
            .map(|v| v.to_string().len())
            .sum();
        assert_eq!(cost.total(), total);
        assert_eq!(estimate_request(&request), total.div_ceil(4) as u64);
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

    pub fn complete(&mut self, estimate: u64, predicted: u64, prompt: Option<u64>, anchor: usize) {
        self.prompt = prompt.unwrap_or(predicted);
        self.request_estimate = estimate;
        self.reported = prompt.is_some();
        self.anchor = anchor;
    }

    pub fn current(&self, session: &Session) -> (u64, bool) {
        let added = session.weigh_from(self.anchor, evidence::estimate) as u64;
        (
            self.prompt.saturating_add(added),
            !self.reported || session.message_count() > self.anchor,
        )
    }

    pub fn record_prompt(&mut self, tokens: u64, anchor: usize) {
        self.prompt = tokens;
        self.anchor = anchor;
        self.reported = true;
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
    fn calibration_allows_context_to_shrink_after_projection() {
        let mut meter = ContextMeter::default();
        meter.complete(1000, 1000, Some(800), 0);
        assert_eq!(meter.predict(1200), 1000);
        assert_eq!(meter.predict(500), 300);
        assert_eq!(ContextMeter::default().predict(500), 500);
    }
}
