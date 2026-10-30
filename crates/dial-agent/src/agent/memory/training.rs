//! Bounded snapshots of the requests that actually produced learnable assistant targets.
use serde_json::{Value, json};

const MAX_EXAMPLES: usize = 8;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(in crate::agent) struct Capture {
    examples: Vec<Value>,
    bytes: usize,
    source_turn: Option<String>,
}

impl Capture {
    pub fn clear(&mut self) {
        self.examples.clear();
        self.bytes = 0;
        self.source_turn = None;
    }

    pub fn set_source_turn(&mut self, entry_id: Option<&str>) {
        self.source_turn = entry_id.filter(|id| !id.is_empty()).map(str::to_owned);
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

impl crate::agent::Agent<'_> {
    /// Preserve the existing logging policy, adding only ordinary durable session snapshots.
    pub(in crate::agent) fn capture_training_requests(&self) -> bool {
        self.logging || self.retain_training_requests()
    }

    fn retain_training_requests(&self) -> bool {
        !self.hermetic && self.domain_override.is_none() && self.session.owns_journal()
    }

    /// Retention is an observation, not a quality verdict or permission to train.
    pub(in crate::agent) fn retain_training_turn(
        &mut self,
        report: &crate::agent::TurnReport,
        prompt: &str,
        ui: &mut dyn crate::agent::Ui,
    ) {
        if !self.retain_training_requests()
            || report.outcome != crate::agent::TurnOutcome::Completed
            || report.interrupted
            || self.training_capture.examples.is_empty()
        {
            return;
        }
        let Some(source_turn) = self.training_capture.source_turn.as_deref() else {
            return;
        };
        let selection = self.effective_config().selection();
        let mut record = json!({
            "version": 1,
            "source_session": self.session.id(),
            "source_turn": source_turn,
            "source_model": format!("{}/{}", selection.provider, selection.model),
            "payload": self.training_capture.payload(),
            "prompt": prompt,
        });
        if let (Some(_), Some(trajectory)) = (self.store, report.trajectory) {
            record["source_state"] = json!(self.config.state_dir);
            record["source_trajectory"] = json!(trajectory);
        }
        match self.session.append_custom_entry("training_turn", record) {
            Ok(commit) => {
                if let Some(error) = commit.durability_error {
                    ui.diagnostic(&format!(
                        "Training observation was appended but durable acknowledgement failed: {error}. No retry was attempted."
                    ));
                }
            }
            Err(error) => ui.diagnostic(&format!(
                "Training observation was not saved: {error:#}. No retry was attempted."
            )),
        }
    }
}

#[cfg(test)]
mod tests;
