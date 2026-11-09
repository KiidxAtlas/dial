//! `shell.undo_snapshot`: copy the project before a shell command that may change files and
//! record what the command changed as checkpoint entries, so `/undo` restores them.
use super::*;
use crate::storage::checkpoint::ShellCoverage;

impl Agent<'_> {
    fn shell_undo(&self) -> Option<std::sync::Arc<crate::storage::checkpoint::Checkpoints>> {
        (!self.hermetic && self.config.shell.undo_snapshot)
            .then(|| self.checkpoints.clone())
            .flatten()
    }

    /// Before a reply's calls run: take the turn's pre-image when one of them is a shell command
    /// that may change files. Says once per turn why a pre-image could not be taken.
    pub(super) fn snapshot_before_shell(&mut self, calls: &[ToolCall], ui: &mut dyn Ui) {
        let opaque = |call: &ToolCall| {
            call.name == "bash"
                && call.arguments["command"]
                    .as_str()
                    .is_some_and(crate::agent::safety::approval::opaque_shell_effect)
        };
        let Some(checkpoints) = self.shell_undo() else {
            return;
        };
        if !calls.iter().any(opaque) {
            return;
        }
        if let ShellCoverage::Unavailable(reason) = checkpoints.snapshot_for_shell()
            && !self.step_checks.shell_undo_unavailable_noted
        {
            self.step_checks.shell_undo_unavailable_noted = true;
            ui.diagnostic(&format!(
                "Shell undo: no copy of the project was taken this turn ({reason}); shell changes stay outside /undo."
            ));
        }
    }

    /// After a shell command, and once more when the turn ends: files that differ from the
    /// pre-image join the turn's checkpoint. A failure leaves them outside `/undo` and says so.
    pub(in crate::agent) fn record_shell_changes(&mut self, ui: &mut dyn Ui) {
        let Some(checkpoints) = self.shell_undo() else {
            return;
        };
        if let Err(error) = checkpoints.record_shell_changes() {
            ui.diagnostic(&format!(
                "Shell undo: changes of this command were not recorded and stay outside /undo: {error:#}"
            ));
        }
    }
}
