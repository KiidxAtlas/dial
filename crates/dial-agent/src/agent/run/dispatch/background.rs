//! Admission of finite native tool jobs. Only owned execution crosses into the background owner;
//! schemas, portable rules, hooks and exact-call approvals stay on the live controller.
use super::{CallbackSource, DispatchMode};
use crate::agent::*;

impl Agent<'_> {
    pub(super) fn start_background_tool(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let request = match tools::exec::background::tool_call_request(&call.arguments) {
            Ok(Some(request)) => request,
            Ok(None) => return ToolOutput::err("bg start requires a tool call"),
            Err(error) => return ToolOutput::err(error),
        };
        let catalog = if self.config.portable_rules.enabled {
            self.layout
                .as_ref()
                .map(|layout| {
                    crate::agent::safety::rules::Catalog::discover(
                        layout,
                        self.workspace.root(),
                        &self.config.portable_rules,
                    )
                })
                .unwrap_or_default()
        } else {
            crate::agent::safety::rules::Catalog::default()
        };
        for warning in &catalog.warnings {
            ui.notice(warning);
        }
        let mut state = crate::agent::safety::rules::State::restore(&self.session.control_state());
        let mut regenerations = state.regenerations();
        let mut gate = crate::agent::safety::rules::CallbackGate::new(
            crate::agent::safety::rules::CallbackRules {
                catalog: &catalog,
                state: &mut state,
                agent: "main",
                regenerations: &mut regenerations,
                max_regenerations: self.config.portable_rules.max_regenerations,
            },
        );
        self.code_callback_mode(
            backend,
            crate::tools::integrations::ext::ToolCallback::Call {
                name: request.tool,
                arguments: request.arguments,
            },
            ui,
            &mut gate,
            CallbackSource {
                owner: "bg",
                provenance,
                mode: DispatchMode::Background { name: request.name },
            },
        )
    }

    pub(super) fn launch_background_tool(
        &mut self,
        call: &ToolCall,
        name: Option<String>,
    ) -> ToolOutput {
        let Some(tool) = self.ext.get(&call.name).cloned() else {
            return ToolOutput::err("background operation is no longer available");
        };
        // Do not capture the bg registry: that would retain the job owner while a provider runs.
        let workspace = self.workspace.clone();
        let config = self.effective_config().clone();
        let call = call.clone();
        let job = tools::exec::background::ToolJob {
            name,
            command: format!("tool: {}", call.name),
            work: Box::new(move |cancel| {
                let tool = tool.fork_background(cancel.clone()).unwrap_or(tool);
                let mut registry = Registry::new();
                registry.register(tool);
                // No borrowed mutation or confirmation authority survives the launch. If an
                // opted-in operation discovers another prerequisite it must fail closed.
                tools::exec::execution::native_read(
                    &ToolContext {
                        workspace: &workspace,
                        config: &config,
                        mutation_authorizer: None,
                        confirm: None,
                    },
                    &registry,
                    &call.name,
                    &call.arguments,
                    &|_| {},
                    &cancel,
                )
                .unwrap_or_else(|| ToolOutput::err("background native operation is unavailable"))
            }),
        };
        match tools::exec::execution::adopt_into(
            &self.ext,
            tools::exec::background::AdoptedJob::Tool(job),
        ) {
            Ok(name) => ToolOutput::structured(json!({"name":name,"state":"running"})),
            Err(error) => ToolOutput::err(error),
        }
    }
}

#[cfg(test)]
mod tests;
