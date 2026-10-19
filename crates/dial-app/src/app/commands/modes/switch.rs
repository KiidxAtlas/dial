//! `/switch [model]`: session-only model override. The config file is never written.

use crate::agent::Agent;
use crate::agent::modes::switch::{parse_target, resolve};
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    if args.trim().is_empty() {
        let selection = agent.effective_config().selection();
        let text = if selection.provider.is_empty() || selection.model.is_empty() {
            "No model is currently selected.".to_owned()
        } else {
            format!("Current model: {}/{}", selection.provider, selection.model)
        };
        shell.note("Switch", text);
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    let target = parse_target(args);
    let selection = match resolve(agent.effective_config(), &target.selector) {
        Ok(selection) => selection,
        Err(error) => {
            shell.note("Switch", format!("{error:#}"));
            return Ok(Step::Done);
        }
    };
    match agent.switch_model(&selection, target.effort) {
        Ok(()) => shell.note(
            "Switch",
            format!(
                "Session-only model: {}/{}.",
                selection.provider, selection.model
            ),
        ),
        Err(error) => shell.note("Switch", format!("Failed to switch model: {error:#}")),
    }
    Ok(Step::Done)
}
