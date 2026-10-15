//! Runs the `security_scan` tool inside the agent loop. Scan, start, and validate drive reviewer
//! children through the backend of the running turn, the same runtime `task` uses, so the tool
//! works on the model path and not only from the `/security` command.

use std::sync::atomic::Ordering;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Agent, Backend, Ui};
use crate::security::Reviewer;
use crate::tools::ToolOutput;

/// Reviewer that runs each request as a `reviewer` child of the agent loop.
struct LoopReviewer<'r, 'a> {
    agent: &'r mut Agent<'a>,
    backend: &'r mut Backend,
    ui: &'r mut dyn Ui,
}

impl Reviewer for LoopReviewer<'_, '_> {
    fn run_reviewer(&mut self, prompt: &str, output_schema: &Value) -> Result<String> {
        let output = self.agent.run_task(
            self.backend,
            &json!({"agent": "reviewer", "task": prompt, "solution_space": "security review of a given scope: findings are open", "output_schema": output_schema}),
            self.ui,
        );
        if output.is_error {
            bail!("{}", output.text);
        }
        Ok(output.text)
    }

    fn interrupted(&self) -> bool {
        self.agent.interrupt.load(Ordering::SeqCst)
    }
}

impl Agent<'_> {
    /// Executes one `security_scan` call against the running turn's backend and UI.
    pub(super) fn run_security_scan(
        &mut self,
        backend: &mut Backend,
        args: &Value,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let config = self.config;
        let mut reviewer = LoopReviewer {
            agent: self,
            backend,
            ui,
        };
        crate::security::tool::run_with(config, args, Some(&mut reviewer))
    }
}
