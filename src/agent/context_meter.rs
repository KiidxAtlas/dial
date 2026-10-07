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

pub(super) fn estimate_request(request: &Chat) -> u64 {
    // Include system text, message envelopes, tool schemas and image descriptors.
    // Images and tokenizer differences make this an approximation, never billed usage.
    (request
        .messages
        .iter()
        .map(|m| m.to_string().len())
        .sum::<usize>()
        + request
            .tools
            .iter()
            .map(|t| t.to_string().len())
            .sum::<usize>())
    .div_ceil(4) as u64
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
}
impl StreamingContext {
    pub fn new(prompt: u64) -> Self {
        Self { prompt, bytes: 0 }
    }
    pub fn observe(&mut self, text: &str) -> u64 {
        self.bytes = self.bytes.saturating_add(text.len() as u64);
        self.prompt.saturating_add(self.bytes.div_ceil(4))
    }
    pub fn finish(&self, prompt: Option<u64>, output: Option<u64>) -> (u64, bool) {
        (
            prompt
                .unwrap_or(self.prompt)
                .saturating_add(output.unwrap_or(self.bytes.div_ceil(4))),
            prompt.is_none() || output.is_none(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
