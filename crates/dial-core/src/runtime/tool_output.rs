//! The result value every tool returns: model-facing text, optional structured data, and the
//! execution facts the controller decides from. Tool implementations and the storage layer both
//! produce and consume it, so it lives with the other core execution types.
use super::execution::{Effects, Execution, ToolStatus};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Original structured result for clients; never recover richer objects by parsing `text`.
    pub data: Option<Value>,
    /// Execution metadata is independent of tool payload, hooks and presentation.
    pub execution: Execution,
}

impl ToolOutput {
    /// Attach validated evidence without replacing another producer's data or execution facts.
    pub fn with_evidence(
        mut self,
        evidence: super::evidence::EvidenceBundle,
    ) -> anyhow::Result<Self> {
        evidence.validate()?;
        let data = self.data.get_or_insert_with(|| serde_json::json!({}));
        let object = data
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("evidence requires object-shaped tool data"))?;
        anyhow::ensure!(
            !object.contains_key("evidence"),
            "tool data already contains evidence"
        );
        object.insert("evidence".into(), serde_json::to_value(evidence)?);
        Ok(self)
    }

    /// Read a validated envelope. Absence is compatible with older results; malformed data fails.
    pub fn evidence(&self) -> anyhow::Result<Option<super::evidence::EvidenceBundle>> {
        let Some(value) = self.data.as_ref().and_then(|data| data.get("evidence")) else {
            return Ok(None);
        };
        let evidence: super::evidence::EvidenceBundle = serde_json::from_value(value.clone())?;
        evidence.validate()?;
        Ok(Some(evidence))
    }
    pub fn with_effects(mut self, effects: Effects) -> Self {
        self.execution.effects = effects;
        if effects == Effects::Unknown {
            self.is_error = true;
            const NOTE: &str = "\n[Effects unknown: work may have partially completed. Inspect the affected state before retrying; interruption does not imply rollback.]";
            if !self.text.ends_with(NOTE) {
                self.text.push_str(NOTE);
            }
        }
        self
    }
    /// Annotate a controller-created outcome without interpreting arbitrary prose.
    pub fn process_outcome(&self) -> Option<crate::runtime::execution::ProcessOutcome> {
        self.execution.process.clone().or_else(|| {
            self.data
                .as_ref()?
                .get("process")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
        })
    }
    pub fn with_process(mut self, process: crate::runtime::execution::ProcessOutcome) -> Self {
        self.execution.process = Some(process.clone());
        let data = self.data.get_or_insert_with(|| serde_json::json!({}));
        if let Some(data) = data.as_object_mut() {
            data.insert("process".into(), serde_json::json!(process));
        }
        self
    }
    pub fn with_status(mut self, status: ToolStatus) -> Self {
        self.execution.status = Some(status);
        self.is_error |= status != ToolStatus::Ok;
        self
    }

    pub fn with_observation(
        mut self,
        kind: super::execution::ObservationKind,
        satisfied: bool,
    ) -> Self {
        self.execution.observation = Some(super::execution::Observation { kind, satisfied });
        self
    }

    pub fn with_output_integrity(mut self, output: super::execution::OutputIntegrity) -> Self {
        self.execution.output = Some(output);
        self
    }

    pub fn with_changes(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.execution.changed_paths = paths.into_iter().collect();
        self
    }

    pub fn structured(data: Value) -> Self {
        Self {
            text: data.to_string(),
            is_error: false,
            data: Some(data),
            execution: Default::default(),
        }
    }
    pub fn ok(text: String) -> Self {
        Self {
            data: None,
            text,
            is_error: false,
            execution: Default::default(),
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            data: None,
            text: text.into(),
            is_error: true,
            execution: Default::default(),
        }
    }
}

impl ToolStatus {
    pub fn from_output(output: &ToolOutput) -> Self {
        output.execution.status.unwrap_or(if output.is_error {
            Self::Error
        } else {
            Self::Ok
        })
    }
}

#[cfg(test)]
mod tests;
