//! Directory access is a capability change, not an ordinary auto-approvable tool call.
//! The existing Ui approval bridge supplies a binary popup, scoped to the canonical directory.
use crate::agent::*;

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":"request_directory","description":"Ask the user through a yes/no popup to add an existing directory, then retry the failed tool. No slash command is needed. Normal access preserves all tool approvals, sandbox and protected storage rules. For protected Dial storage, set sensitive=true to request a SECOND, separate native-file-only approval for that exact directory; shell protection and credential secrecy remain. Grants are never automatic, even in auto modes. Sensitive grants last only in this process.","parameters":{"type":"object","properties":{"path":{"type":"string","minLength":1},"reason":{"type":"string","minLength":1},"sensitive":{"type":"boolean","default":false}},"required":["path","reason"],"additionalProperties":false}}})
}

impl Agent<'_> {
    pub(in crate::agent) fn request_directory(
        &mut self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let result = self.request_directory_inner(call, ui);
        match result {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::err(format!("{error:#}"))
                .with_status(tools::ToolStatus::Blocked)
                .with_effects(crate::tools::surface::outcome::Effects::None),
        }
    }

    fn request_directory_inner(&mut self, call: &ToolCall, ui: &mut dyn Ui) -> Result<String> {
        ensure!(
            !self.hermetic && ui.interactive_input(),
            "directory access requires an interactive human approval; unavailable in unattended or hermetic runs"
        );
        ensure!(
            self.tool_eligible("request_directory"),
            "directory access is unavailable in this mode"
        );
        let path = call.arguments["path"]
            .as_str()
            .context("path is required")?;
        let reason = call.arguments["reason"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .context("reason is required")?;
        let sensitive = call.arguments["sensitive"].as_bool().unwrap_or(false);
        let canonical = self
            .workspace
            .directory_request(Path::new(path), sensitive)?;
        let ask = |ui: &mut dyn Ui, tool, summary: &str| {
            matches!(
                ui.approve_call(&call.id, tool, summary, Some(&self.interrupt)),
                Decision::Allow
            )
        };
        ensure!(
            ask(
                ui,
                "request_directory",
                &format!(
                    "Add directory for this session?\nCanonical directory: {}\nReason: {}\nNormal read/write and sandboxed command access. Existing tool approvals, credential and protected Dial storage restrictions remain. This does NOT approve sensitive access.",
                    canonical.display(),
                    reason
                )
            ),
            "directory access denied"
        );
        if sensitive {
            ensure!(
                ask(
                    ui,
                    "request_sensitive_directory",
                    &format!(
                        "Separate sensitive access approval\nCanonical directory: {}\nReason: {}\nAllow native file tools to read AND write protected Dial storage inside ONLY this directory for this process? Shell commands remain blocked by sandbox storage protection. Credentials remain inaccessible. This grant is not restored on resume.",
                        canonical.display(),
                        reason
                    )
                ),
                "sensitive directory access denied; no access changed"
            );
        }
        ensure!(
            !self.interrupt.load(Ordering::SeqCst),
            "directory approval cancelled"
        );
        // Revalidate the displayed canonical identity after the popup; never follow a swapped alias.
        ensure!(
            self.workspace
                .directory_request(Path::new(path), sensitive)?
                == canonical,
            "requested directory changed during approval; request access again"
        );
        let previous = self.workspace.clone();
        if sensitive {
            self.workspace.grant_sensitive_directory(&canonical)?;
        } else if canonical != self.workspace.root() {
            self.workspace.add_root(&canonical)?;
        }
        if let Err(error) = self.persist_workspace_roots() {
            self.workspace = previous;
            return Err(error);
        }
        self.rebuild_system();
        Ok(format!(
            "Added {}. Retry the original tool. {}",
            canonical.display(),
            if sensitive {
                "Sensitive native-file access granted for this process only; shell protection and credential exclusions remain."
            } else {
                "Normal tool approvals, sandbox and protected storage restrictions remain."
            }
        ))
    }
}

#[cfg(test)]
mod tests;
