//! Turn-local enforcement of corrective feedback. Notices alone are not a control plane.
use crate::llm::ToolCall;
use std::collections::BTreeMap;

#[derive(Default)]
pub(in crate::agent) struct RecoveryCheckpoint {
    withheld: BTreeMap<String, Withheld>,
    cached_observations: BTreeMap<String, u64>,
    source_versions: BTreeMap<String, SourceVersion>,
}

#[derive(PartialEq, Eq)]
enum SourceVersion {
    Read(Vec<Option<[u8; 32]>>),
    Shell(String),
}

/// One withheld call's feedback and the files it depends on. `None` means the dependency is not
/// known (for example a shell command), so any successful mutation releases it. Known files are
/// released only by a committed change to one of them.
struct Withheld {
    reason: String,
    targets: Option<Vec<std::path::PathBuf>>,
}

fn control(name: &str) -> bool {
    matches!(
        name,
        "todo" | "finish" | "working_state" | "tool_search" | "tool_load"
    )
}

/// Resolved files a call reads, edits or writes. Empty for other tools and whenever any named
/// path does not resolve, so a partly unknown call keeps the conservative unknown-dependency rule.
pub(in crate::agent) fn declared_targets(
    ws: &crate::tools::Workspace,
    call: &ToolCall,
) -> Vec<std::path::PathBuf> {
    let resolved = match call.name.as_str() {
        "read" => call.arguments["path"]
            .as_str()
            .into_iter()
            .chain(
                call.arguments["paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str),
            )
            .map(|path| ws.resolve_read_target(path))
            .collect::<Result<Vec<_>, _>>(),
        "edit" => crate::tools::declared_edit_targets(&call.arguments)
            .unwrap_or_default()
            .iter()
            .map(|path| ws.resolve(path))
            .collect::<Result<Vec<_>, _>>(),
        "write" => call.arguments["path"]
            .as_str()
            .map(|path| ws.resolve(path))
            .into_iter()
            .collect::<Result<Vec<_>, _>>(),
        _ => return Vec::new(),
    };
    resolved.unwrap_or_default()
}

impl RecoveryCheckpoint {
    pub fn observe_at(
        &mut self,
        ws: &crate::tools::Workspace,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
        investigation: &mut crate::agent::workflow::investigation::InvestigationGuard,
        repeats: &mut crate::agent::safety::guard::RepeatGuard,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let result = self.observe(call, output, investigation, repeats);
        let key = Self::key(call);
        if let Some(entry) = self.withheld.get_mut(&key) {
            let targets = declared_targets(ws, call);
            entry.targets = (!targets.is_empty()).then_some(targets);
        }
        if self.withheld.contains_key(&key)
            && let Some(version) = Self::source_version(ws, call)
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
            && Self::source_version(ws, call).is_some_and(|current| &current != previous)
        {
            self.withheld.remove(&key);
            self.cached_observations.remove(&key);
            self.source_versions.remove(&key);
        }
        self.rejection(call)
    }
    fn source_version(ws: &crate::tools::Workspace, call: &ToolCall) -> Option<SourceVersion> {
        match call.name.as_str() {
            "read" => Self::read_versions(ws, call).map(SourceVersion::Read),
            "bash" => {
                crate::agent::verify::shared_runs::shell_tree(ws, call).map(SourceVersion::Shell)
            }
            _ => None,
        }
    }

    /// Keep every target in a batched read, including unavailable identities: a target becoming
    /// readable or disappearing must release the stale restriction too. These hashes only expire
    /// feedback; they never authorize access or establish verification evidence.
    fn read_versions(
        ws: &crate::tools::Workspace,
        call: &ToolCall,
    ) -> Option<Vec<Option<[u8; 32]>>> {
        if let Some(path) = call.arguments["path"].as_str() {
            return Some(vec![ws.read_identity(path)]);
        }
        call.arguments["paths"]
            .as_array()?
            .iter()
            .map(|path| path.as_str().map(|path| ws.read_identity(path)))
            .collect()
    }

    /// Shared main/child decision: normalize once, preserve observation history,
    /// withhold only this call, and leave corrected arguments eligible.
    pub fn observe(
        &mut self,
        call: &ToolCall,
        output: &crate::runtime::tool_output::ToolOutput,
        investigation: &mut crate::agent::workflow::investigation::InvestigationGuard,
        repeats: &mut crate::agent::safety::guard::RepeatGuard,
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
                crate::agent::workflow::investigation::observation_key(call, &output.text),
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
                && !self.withheld.contains_key(&Self::key(call))
                && let Some((key, _)) = self.withheld.pop_first()
            {
                self.cached_observations.remove(&key);
                self.source_versions.remove(&key);
            }
            self.withheld.insert(
                Self::key(call),
                Withheld {
                    reason: format!(
                        "`{}`: {}",
                        call.name,
                        advice.chars().take(700).collect::<String>()
                    ),
                    targets: None,
                },
            );
        }
    }

    fn key(call: &ToolCall) -> String {
        format!(
            "{}\0{}",
            call.name,
            crate::agent::safety::guard::normalized_arguments(call)
        )
    }

    /// Release feedback for a call whose missing access was explicitly granted.
    pub fn release_key(&mut self, key: &str) {
        self.withheld.remove(key);
        self.cached_observations.remove(key);
        self.source_versions.remove(key);
    }

    pub fn rejection(&self, call: &ToolCall) -> Option<String> {
        self.withheld.get(&Self::key(call)).map(|entry| format!(
            "Recovery checkpoint: this `{}` call is temporarily withheld after repeated unsuccessful or unchanged actions. This call was not executed. Other arguments and narrower observations remain available; repair the known problem, reuse existing evidence, or report a blocker. Previous feedback: {}", call.name, entry.reason
        ))
    }

    /// A resolved todo releases all withheld feedback. A novel successful mutation releases only
    /// feedback with an unknown dependency, or with a known file it changed; feedback about other
    /// known files stays withheld.
    pub fn progress(
        &mut self,
        call: &ToolCall,
        success: bool,
        todo_changed: bool,
        mutated: bool,
        changed: &[std::path::PathBuf],
    ) {
        let task_resolved = call.name == "todo"
            && todo_changed
            && matches!(
                crate::agent::workflow::todo::requested_op(&call.arguments),
                Some("done" | "block")
            );
        if !success {
            return;
        }
        if task_resolved {
            self.withheld.clear();
            self.cached_observations.clear();
            self.source_versions.clear();
        } else if mutated {
            self.withheld.retain(|_, entry| {
                entry
                    .targets
                    .as_ref()
                    .is_some_and(|targets| !targets.iter().any(|target| changed.contains(target)))
            });
            let withheld = &self.withheld;
            self.cached_observations
                .retain(|key, _| withheld.contains_key(key));
            self.source_versions
                .retain(|key, _| withheld.contains_key(key));
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
