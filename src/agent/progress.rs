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
        if let Some(cached) = value.cached_tokens {
            self.value.cached_tokens = Some(cached);
        }
        if value.eta_secs.is_some() {
            self.value.eta_secs = value.eta_secs;
        } else {
            self.prompt_eta(now);
        }
        if let Some(tokens) = value
            .output_tokens
            .filter(|_| !(self.exact && value.estimated))
        {
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
            self.value.eta_secs = None;
            self.first = None;
        }
        if !self.exact {
            self.bytes = self.bytes.saturating_add(text.len() as u64);
            self.value.output_tokens = Some(self.bytes.div_ceil(4));
            self.value.estimated = true;
            self.rate(now);
        }
    }

    /// Seconds of prompt processing left, from the rate measured since the first processing report
    /// of this request (cached tokens are skipped by the server and do not count toward the rate).
    fn prompt_eta(&mut self, now: Instant) {
        if self.value.phase != InferencePhase::Processing {
            return;
        }
        let (Some(total), Some(done)) = (self.value.prompt_tokens, self.value.processed_tokens)
        else {
            return;
        };
        let (start, first) = *self.first.get_or_insert((now, done));
        let seconds = now.saturating_duration_since(start).as_secs_f64();
        self.value.eta_secs = (seconds > 0.0 && done > first && total > done)
            .then(|| (total - done) as f64 * seconds / (done - first) as f64);
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

    #[test]
    fn prompt_eta_follows_the_measured_processing_rate() {
        let now = Instant::now();
        let mut tracker = RequestProgress::new();
        let report = |done: u64, cached: Option<u64>| {
            let mut value = ModelProgress::new(InferencePhase::Processing);
            value.prompt_tokens = Some(10_000);
            value.processed_tokens = Some(done);
            value.cached_tokens = cached;
            value
        };
        tracker.observe(report(2_000, Some(2_000)), now);
        assert_eq!(tracker.value.eta_secs, None, "no rate after one report");
        assert_eq!(tracker.value.cached_tokens, Some(2_000));
        tracker.observe(report(4_000, None), now + Duration::from_secs(2));
        // 2000 tokens in 2 s leaves 6000 tokens: 6 s.
        assert_eq!(tracker.value.eta_secs, Some(6.0));
        assert_eq!(tracker.value.cached_tokens, Some(2_000));
        tracker.text("first", now + Duration::from_secs(3));
        assert_eq!(
            tracker.value.eta_secs, None,
            "generation clears the prompt ETA"
        );
    }
}
