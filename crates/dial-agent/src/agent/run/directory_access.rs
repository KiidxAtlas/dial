//! Directory access is a capability change, not an ordinary auto-approvable tool call.
//! The existing Ui approval bridge supplies a binary popup, scoped to the canonical directory.
use crate::agent::*;

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":"request_directory","description":"Ask the user through a yes/no popup to add an existing directory. No slash command is needed. Normal access preserves all tool approvals, sandbox and protected storage rules. For protected Dial storage, set sensitive=true to request a SECOND, separate native-file-only approval for that exact directory; shell protection and credential secrecy remain. Grants are never automatic, even in auto modes. Sensitive grants last only in this process.","parameters":{"type":"object","properties":{"path":{"type":"string","minLength":1},"reason":{"type":"string","minLength":1},"sensitive":{"type":"boolean","default":false}},"required":["path","reason"],"additionalProperties":false}}})
}

/// Only declared native filesystem targets can open a popup. Never infer a permission
/// request from a tool's error text, command operands, or an extension's arguments.
pub(in crate::agent) fn pending_directories(
    workspace: &Workspace,
    registry: &crate::tools::integrations::ext::Registry,
    name: &str,
    args: &Value,
) -> Vec<(PathBuf, bool)> {
    let mut directories = Vec::new();
    if registry
        .origin(name)
        .is_some_and(|origin| origin != tools::surface::catalog::Origin::Native)
    {
        return directories;
    }
    let mut add = |path: &str| {
        if let Some(request) = workspace.pending_directory_access(path, name)
            && !directories.contains(&request)
        {
            directories.push(request);
        }
    };
    match name {
        "read" => {
            for path in args["path"].as_str().into_iter().chain(
                args["paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            ) {
                add(path);
            }
        }
        "grep" | "glob" | "find" => {
            if let Some(scope) = args["path"].as_str() {
                for path in scope.split(';') {
                    add(path.trim());
                }
            }
        }
        "edit" | "write" => {
            let paths = if name == "write" {
                tools::declared_write_targets(args)
            } else {
                tools::editing_targets(workspace.edit_mode(), args)
            };
            if let Ok(paths) = paths {
                for path in paths {
                    add(&path);
                }
            }
        }
        "bash" | "bg" if name == "bash" || args["action"] == "start" => {
            if let Some(cwd) = args["cwd"].as_str() {
                add(cwd);
            }
        }
        _ => {}
    }
    directories
}

impl Agent<'_> {
    /// Run the existing capability prompt on the original call's id, before execution, not
    /// in response to a failed operation (which could already have had partial effects).
    pub(in crate::agent) fn approve_pending_directories(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        for (directory, sensitive) in
            pending_directories(&self.workspace, &self.ext, &call.name, &call.arguments)
        {
            let request = ToolCall {
                id: call.id.clone(),
                name: "ask".into(),
                arguments: json!({"action":"directory", "path": directory, "reason":format!("Continue the pending {} call", call.name), "sensitive":sensitive}),
                malformed_arguments: false,
            };
            // Go through the facade's normal dispatch: a hook veto, disabled controller
            // operation or permission rule must block inline grants just like explicit ones.
            let outcome = self.dispatch(backend, &request, ui);
            ensure!(!outcome.is_error, "{}", outcome.text);
            ensure!(
                !self.interrupt.load(Ordering::SeqCst),
                "directory approval cancelled"
            );
        }
        Ok(())
    }

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
        // Only an exact one-time Allow grants. A reusable answer ("always"/"project") from a client
        // that offered those buttons is refused too, but says why, so the user can answer again.
        let ask = |ui: &mut dyn Ui, tool, summary: &str, denied: &str| -> Result<()> {
            match ui.approve_call(&call.id, tool, summary, Some(&self.interrupt)) {
                Decision::Allow => Ok(()),
                Decision::AllowAlways | Decision::AllowProject => bail!(
                    "{denied}: directory access is granted once per request, so an \"always\" or \"project\" answer cannot grant it. Request access again and choose Allow (once)"
                ),
                Decision::Deny => bail!("{denied}"),
            }
        };
        ask(
            ui,
            "request_directory",
            &format!(
                "Add directory for this session?\nCanonical directory: {}\nReason: {}\nNormal read/write and sandboxed command access. Existing tool approvals, credential and protected Dial storage restrictions remain. This does NOT approve sensitive access.",
                canonical.display(),
                reason
            ),
            "directory access denied",
        )?;
        if sensitive {
            ask(
                ui,
                "request_sensitive_directory",
                &format!(
                    "Separate sensitive access approval\nCanonical directory: {}\nReason: {}\nAllow native file tools to read AND write protected Dial storage inside ONLY this directory for this process? Shell permissions do not change: sandboxed shells remain blocked; an unconfined shell is not protected. Native tools still exclude credentials. This grant is not restored on resume.",
                    canonical.display(),
                    reason
                ),
                "sensitive directory access denied; no access changed",
            )?;
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
            "Added {}. {}",
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
