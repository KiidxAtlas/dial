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

    /// Record the request exactly as `form` sent it, then the target. Dispatch already applied
    /// the same projection to these messages, so a failure here means they diverged: skip the
    /// example rather than train on a request the model never saw.
    pub fn record(
        &mut self,
        context: &[Value],
        tools: &[Value],
        target: Value,
        form: crate::llm::messages::WireForm,
    ) {
        let Ok(sent) = form.apply(context) else {
            return;
        };
        let mut messages: Vec<_> = sent.iter().map(crate::llm::messages::canonical).collect();
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

    /// Keep each exact wire schema list once; descriptions can change without changing a tool
    /// contract, so its complete wire hash is the lossless reference identity.
    fn retained_payload(&self) -> Value {
        use sha2::{Digest, Sha256};
        let mut tools = serde_json::Map::new();
        let examples: Vec<_> = self
            .examples
            .iter()
            .cloned()
            .map(|mut example| {
                if let Some(schema) = example
                    .as_object_mut()
                    .and_then(|item| item.remove("tools"))
                {
                    let key = crate::hex(&Sha256::digest(schema.to_string().as_bytes()));
                    tools.entry(key.clone()).or_insert(schema);
                    example["tools_ref"] = json!(key);
                }
                example
            })
            .collect();
        json!({"examples":examples,"tool_sets":tools})
    }
}

impl crate::agent::Agent<'_> {
    /// Capture opt-in durable teaching requests or explicit hermetic learning logs.
    pub(in crate::agent) fn capture_training_requests(&self) -> bool {
        self.retain_training_requests() || (self.hermetic && self.logging)
    }

    fn retain_training_requests(&self) -> bool {
        self.config.training.retain_session_requests
            && !self.hermetic
            && self.domain_override.is_none()
            && self.session.owns_journal()
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
        let mut payload = self.training_capture.retained_payload();
        let sets = payload
            .as_object_mut()
            .and_then(|payload| payload.remove("tool_sets"))
            .and_then(|sets| sets.as_object().cloned())
            .unwrap_or_default();
        let previous = self.session.custom_records("training_tool_sets");
        let fresh: serde_json::Map<String, Value> = sets
            .into_iter()
            .filter(|(key, tools)| {
                !previous
                    .iter()
                    .any(|(_, record)| record["tool_sets"].get(key) == Some(tools))
            })
            .collect();
        if !fresh.is_empty() {
            match self
                .session
                .append_custom_entry("training_tool_sets", json!({"version":1,"tool_sets":fresh}))
            {
                Ok(commit) if commit.durability_error.is_none() => {}
                Ok(commit) => {
                    ui.diagnostic(&format!("Training schemas were appended but durable acknowledgement failed: {:?}. No turn snapshot was saved.", commit.durability_error));
                    return;
                }
                Err(error) => {
                    ui.diagnostic(&format!(
                        "Training schemas were not saved: {error:#}. No turn snapshot was saved."
                    ));
                    return;
                }
            }
        }
        let selection = self.effective_config().selection();
        let mut record = json!({
            "version": 3,
            "source_session": self.session.id(),
            "source_turn": source_turn,
            "source_model": format!("{}/{}", selection.provider, selection.model),
            "payload": payload,
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
