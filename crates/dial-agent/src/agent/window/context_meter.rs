//! Display-only context telemetry. Provider accounting remains separate from estimates.
use crate::agent::workflow::evidence;
use crate::{
    llm::{Chat, wire::json_bytes},
    storage::session::Session,
};

#[derive(Default)]
pub(in crate::agent) struct ContextMeter {
    prompt: u64,
    request_estimate: u64,
    reported: bool,
    rebased: bool,
    anchor: usize,
}

/// Disjoint serialized byte categories, not tokenizer counts or billed usage.
#[derive(Default, serde::Serialize)]
pub(in crate::agent) struct RequestOverhead {
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

pub(in crate::agent) fn overhead(request: &Chat) -> RequestOverhead {
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
pub(in crate::agent) struct StreamingContext {
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
