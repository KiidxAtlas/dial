//! Request-local display telemetry. Estimated counts never enter billing or conversation history.
use crate::llm::{InferencePhase, ModelProgress};
use std::time::Instant;

pub(super) struct RequestProgress {
    pub value: ModelProgress,
    bytes: u64,
    exact: bool,
    first: Option<(Instant, u64)>,
}

impl RequestProgress {
    pub fn new() -> Self {
        Self {
            value: ModelProgress::new(InferencePhase::Processing),
            bytes: 0,
            exact: false,
            first: None,
        }
    }

    pub fn observe(&mut self, mut value: ModelProgress, now: Instant) {
        value.normalize();
        let previous_tokens = self.value.output_tokens;
        if value.phase != self.value.phase {
            self.first = None;
            self.value = ModelProgress::new(value.phase);
        }
        if let Some(total) = value.prompt_tokens {
            self.value.prompt_tokens = Some(total);
        }
        if let Some(done) = value.processed_tokens {
            self.value.processed_tokens = Some(done);
        }
        if let Some(tokens) = value.output_tokens {
            if !self.exact && !value.estimated {
                self.first = None;
                self.value.tokens_per_second = None;
            }
            // An exact count replaces an estimate, even when it is smaller.
            self.value.output_tokens = Some(if self.exact && !value.estimated {
                self.value.output_tokens.unwrap_or(0).max(tokens)
            } else {
                tokens
            });
            self.exact = !value.estimated;
            self.value.estimated = value.estimated;
        }
        if value.tokens_per_second.is_some() {
            self.value.tokens_per_second = value.tokens_per_second;
        } else if self.value.output_tokens != previous_tokens {
            self.rate(now);
        }
        self.value.normalize();
    }

    pub fn text(&mut self, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        if self.value.phase != InferencePhase::Generating {
            self.value.phase = InferencePhase::Generating;
            self.first = None;
        }
        if !self.exact {
            self.bytes = self.bytes.saturating_add(text.len() as u64);
            self.value.output_tokens = Some(self.bytes.div_ceil(4));
            self.value.estimated = true;
            self.rate(now);
        }
    }

    fn rate(&mut self, now: Instant) {
        if self.value.phase != InferencePhase::Generating {
            return;
        }
        let Some(tokens) = self.value.output_tokens else {
            return;
        };
        let (start, first) = *self.first.get_or_insert((now, tokens));
        let seconds = now.saturating_duration_since(start).as_secs_f64();
        if seconds > 0.0 && tokens > first {
            self.value.tokens_per_second = Some((tokens - first) as f64 / seconds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn live_counts_cover_hidden_output_and_exact_counts_replace_estimates() {
        let now = Instant::now();
        let mut tracker = RequestProgress::new();
        tracker.text("abcdefgh", now);
        tracker.text("ijklmnop", now + Duration::from_secs(1));
        assert_eq!(tracker.value.output_tokens, Some(4));
        assert_eq!(tracker.value.tokens_per_second, Some(2.0));
        assert!(tracker.value.estimated);
        let mut exact = ModelProgress::new(InferencePhase::Generating);
        exact.output_tokens = Some(3);
        tracker.observe(exact.clone(), now + Duration::from_secs(2));
        assert_eq!(tracker.value.output_tokens, Some(3));
        assert!(!tracker.value.estimated);
        tracker.text("tool arguments", now + Duration::from_secs(3));
        assert_eq!(tracker.value.output_tokens, Some(3));
        exact.output_tokens = Some(8);
        exact.tokens_per_second = Some(30.0);
        tracker.observe(exact.clone(), now + Duration::from_secs(4));
        assert_eq!(tracker.value.tokens_per_second, Some(30.0));
        exact.tokens_per_second = None;
        tracker.observe(exact, now + Duration::from_secs(5));
        assert_eq!(
            tracker.value.tokens_per_second,
            Some(30.0),
            "final usage must preserve the engine rate"
        );
        assert_eq!(RequestProgress::new().value.output_tokens, None);
    }
}
