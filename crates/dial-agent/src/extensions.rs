//! Rust embedding SDK. Extensions register tools and commands before a turn; tool execution still
//! passes through the agent's schemas, approvals, confinement and checkpoint observer.
use crate::{
    agent::{Agent, Ui},
    tools::integrations::ext::Tool,
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, sync::Arc};

pub const SDK_VERSION: u32 = 1;
/// Extensions are operator-trusted native code, not executable workspace content.
pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn api_version(&self) -> u32 {
        SDK_VERSION
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<Command> {
        Vec::new()
    }
}
/// A command handler: takes the argument text and the UI, returns the prompt text to expand.
pub type CommandHandler = Arc<dyn Fn(&str, &mut dyn Ui) -> Result<String> + Send + Sync>;
/// Commands may render a panel and expand into a user-visible prompt, not bypass tool execution.
#[derive(Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub execute: CommandHandler,
}
#[derive(Default)]
pub struct ExtensionHost {
    commands: BTreeMap<String, Command>,
    names: Vec<String>,
}
impl ExtensionHost {
    pub fn install(&mut self, agent: &mut Agent<'_>, extension: &dyn Extension) -> Result<()> {
        ensure!(
            extension.api_version() == SDK_VERSION,
            "unsupported extension SDK version"
        );
        let name = extension.name();
        ensure!(
            !name.is_empty() && !self.names.iter().any(|n| n == name),
            "empty or duplicate extension name"
        );
        let tools = extension.tools();
        let commands = extension.commands();
        let mut seen = std::collections::HashSet::new();
        let existing = agent.available_tool_names();
        for tool in &tools {
            crate::tools::surface::catalog::validate_tool(tool.as_ref())?;
            ensure!(
                !existing.iter().any(|n| n == tool.name()) && seen.insert(tool.name().to_owned()),
                "tool name collides: {}",
                tool.name()
            );
        }
        seen.clear();
        for command in &commands {
            ensure!(
                !command.name.is_empty()
                    && command
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid command name"
            );
            ensure!(
                !self.commands.contains_key(&command.name) && seen.insert(command.name.clone()),
                "command name collides: {}",
                command.name
            );
        }
        agent.register_tools(tools)?;
        for command in commands {
            self.commands.insert(command.name.clone(), command);
        }
        self.names.push(name.to_owned());
        Ok(())
    }
    pub fn commands(&self) -> impl Iterator<Item = &Command> {
        self.commands.values()
    }
    pub fn invoke(
        &self,
        agent: &mut Agent<'_>,
        name: &str,
        args: &str,
        ui: &mut dyn Ui,
    ) -> Result<crate::agent::TurnReport> {
        let command = self
            .commands
            .get(name)
            .context("unknown extension command")?;
        let prompt = (command.execute)(args, ui)?;
        ensure!(
            prompt.len() <= 1024 * 1024,
            "extension command prompt exceeds 1 MiB"
        );
        agent.turn(&prompt, ui)
    }
}
