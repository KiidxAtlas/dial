//! `revert_edit`: put one file back to its state before Dial's latest checkpointed change to it in
//! this turn, or before the turn's first change.
//!
//! The checkpoint journal plans the restore and refuses files changed outside it since that change;
//! the edit transaction commits it, so protected paths, path permission rules, the checkpoint
//! observer (the revert is itself undoable with /undo) and committed-path facts apply as for
//! `edit`. Mutation accounting, verification staleness and edit-cycle tracking follow from the
//! committed path like any other edit.
use crate::agent::*;

pub(in crate::agent) const NAME: &str = "revert_edit";

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":NAME,"description":"Restore one file to its content before Dial's most recent change to it in this turn, or before the turn's first change with to=\"turn_start\". Refuses files changed outside Dial since that change. The revert is checkpointed (/undo applies); read the file before editing it again.","parameters":{"type":"object","properties":{"path":{"type":"string"},"to":{"type":"string","enum":["last_change","turn_start"]}},"required":["path"],"additionalProperties":false}}})
}

impl Agent<'_> {
    pub(in crate::agent) fn revert_edit_tool(&mut self, call: &ToolCall) -> ToolOutput {
        let Some(checkpoints) = self.checkpoints.clone() else {
            return ToolOutput::err(
                "revert_edit needs the session checkpoint journal, which is unavailable here",
            );
        };
        let Some(path) = call.arguments["path"].as_str() else {
            return ToolOutput::err("revert_edit requires `path`");
        };
        let turn_start = call.arguments["to"] == "turn_start";
        let target = match self.workspace.resolve(path) {
            Ok(target) => target,
            Err(error) => return ToolOutput::err(error),
        };
        // Edit-family deny rules and scoped grants judge the resolved target, as for native edits.
        if let Err(why) = crate::agent::safety::approval::authorize_mutation_paths(
            crate::agent::safety::approval::MutationPolicy {
                rules: &self.always,
                root: self.workspace.root(),
                extra_roots: self.workspace.additional_roots(),
                read_only: false,
            },
            call,
            NAME,
            std::slice::from_ref(&target),
            false,
        ) {
            return ToolOutput::err(why)
                .with_status(crate::tools::ToolStatus::Blocked)
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
        let plan = match checkpoints.revert_plan(&target, turn_start) {
            Ok(plan) => plan,
            Err(error) => return ToolOutput::err(format!("{error:#}")),
        };
        let label = if turn_start {
            "at the start of this turn"
        } else {
            "before its last change"
        };
        let output = crate::tools::edit::revert::restore(
            &self.workspace,
            &plan.path,
            plan.current,
            plan.content,
            label,
        );
        checkpoints.settle_changes();
        output
    }
}

#[cfg(test)]
mod tests;
