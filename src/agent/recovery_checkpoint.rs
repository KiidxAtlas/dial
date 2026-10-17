//! Turn-local enforcement of corrective feedback. Notices alone are not a control plane.
use crate::llm::ToolCall;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct RecoveryCheckpoint {
    withheld: BTreeMap<String, String>,
    cached_observations: BTreeMap<String, u64>,
    source_versions: BTreeMap<String, [u8; 32]>,
}

fn control(name: &str) -> bool {
    matches!(
        name,
        "todo" | "finish" | "working_state" | "tool_search" | "tool_load" | "artifact_read"
    )
}

impl RecoveryCheckpoint {
    pub fn observe_at(
        &mut self,
        ws: &crate::tools::Workspace,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
        investigation: &mut super::investigation::InvestigationGuard,
        repeats: &mut super::guard::RepeatGuard,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let result = self.observe(call, output, investigation, repeats);
        let key = Self::key(call);
        if call.name == "read"
            && !output.is_error
            && self.withheld.contains_key(&key)
            && let Some(version) = call.arguments["path"]
                .as_str()
                .and_then(|path| ws.read_identity(path))
        {
            self.source_versions.insert(key, version);
        }
        result
    }

    pub fn rejection_at(
        &mut self,
        ws: &crate::tools::Workspace,
        call: &ToolCall,
    ) -> Option<String> {
        let key = Self::key(call);
        if let Some(previous) = self.source_versions.get(&key)
            && call.arguments["path"]
                .as_str()
                .and_then(|path| ws.read_identity(path))
                .as_ref()
                != Some(previous)
        {
            self.withheld.remove(&key);
            self.cached_observations.remove(&key);
            self.source_versions.remove(&key);
        }
        self.rejection(call)
    }
    /// Shared main/child decision: normalize once, preserve observation history,
    /// withhold only this call, and leave corrected arguments eligible.
    pub fn observe(
        &mut self,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
        investigation: &mut super::investigation::InvestigationGuard,
        repeats: &mut super::guard::RepeatGuard,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let key = Self::key(call);
        let recovery_error = output
            .text
            .strip_prefix("invalid tool call: ")
            .unwrap_or(&output.text);
        let duplicate = if output.is_error
            && recovery_error.starts_with("Recovery checkpoint: this")
        {
            self.cached_observations
                .get(&key)
                .and_then(|key| investigation.repeat_known(*key))
                .map(|advice| format!("This call was not executed. Reuse the earlier evidence; corrected arguments and changed sources remain available.\n{advice}"))
        } else {
            investigation.observe_output(call, output)
        };
        // Escalation is stated with the withheld observation it concerns and never ends the turn.
        let duplicate = match (duplicate, investigation.escalation()) {
            (Some(advice), Some(escalation)) => Some(format!("{advice}\n{escalation}")),
            (advice, _) => advice,
        };
        if duplicate.is_some() && !output.is_error {
            self.cached_observations.insert(
                key,
                super::investigation::observation_key(call, &output.text),
            );
        }
        let repeat = if duplicate.is_some() {
            None
        } else {
            repeats.observe(call, output.is_error)
        };
        // Withhold the exact repeated observation consistently. Corrected ranges, other files,
        // and changed sources remain available through their own call/source identities.
        for advice in [duplicate.as_deref(), repeat.as_deref()]
            .into_iter()
            .flatten()
        {
            self.arm(call, advice);
        }
        // Repetition escalates inside the advice above and never ends the turn, so no stop is produced.
        (duplicate, repeat, None)
    }

    pub fn arm(&mut self, call: &ToolCall, advice: &str) {
        if !control(&call.name) {
            if self.withheld.len() >= 32
                && let Some((key, _)) = self.withheld.pop_first()
            {
                self.cached_observations.remove(&key);
                self.source_versions.remove(&key);
            }
            self.withheld.insert(
                Self::key(call),
                format!(
                    "`{}`: {}",
                    call.name,
                    advice.chars().take(700).collect::<String>()
                ),
            );
        }
    }

    fn key(call: &ToolCall) -> String {
        format!(
            "{}\0{}",
            call.name,
            super::guard::normalized_arguments(call)
        )
    }

    pub fn rejection(&self, call: &ToolCall) -> Option<String> {
        self.withheld.get(&Self::key(call)).map(|reason| format!(
            "Recovery checkpoint: this `{}` call is temporarily withheld after repeated unsuccessful or unchanged actions. This call was not executed. Other arguments and narrower observations remain available; repair the known problem, reuse existing evidence, or report a blocker. Previous feedback: {reason}", call.name
        ))
    }

    pub fn apply(
        &self,
        _schemas: &mut Vec<Value>,
        _names: &mut Vec<String>,
        messages: &mut Vec<Value>,
    ) {
        if self.withheld.is_empty() {
            return;
        }
        // Keep tools discoverable: only the unsuccessful call is withheld, not
        // legitimate corrected arguments, another file, or a narrower check.
        let reasons = self
            .withheld
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        messages.push(json!({"role":"user","dial_transient":true,"content":format!(
            "[Recovery checkpoint from Dial]\n{reasons}\nDo not repeat the unchanged calls. The tools remain available with corrected arguments or for genuinely different evidence. Resume the existing task; do not create a new plan or edit merely to restore tool access. If no useful action is available, call finish with a specific blocker or question."
        )}));
    }

    pub fn progress(&mut self, call: &ToolCall, success: bool, todo_changed: bool, mutated: bool) {
        let task_resolved = call.name == "todo"
            && todo_changed
            && matches!(call.arguments["op"].as_str(), Some("done" | "block"));
        if success && (mutated || task_resolved) {
            self.withheld.clear();
            self.cached_observations.clear();
            self.source_versions.clear();
        }
    }

    /// Compaction removed or summarized the results these entries referred to. Withheld calls and
    /// cached observations describe evidence the model may no longer see, so they are released.
    /// The failure counters belong to the repeat guard and are not touched here.
    pub fn forget_evidence(&mut self) {
        self.withheld.clear();
        self.cached_observations.clear();
        self.source_versions.clear();
    }
}

#[cfg(test)]
mod tests;
