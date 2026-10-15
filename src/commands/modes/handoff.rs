//! `/handoff [focus]`: summarize the session into a handoff document and compact in place.
//! Runs the `Handoff` compaction method through the same engine as `/compact`.

use crate::agent::Agent;
use crate::agent::compaction::{CompactionMethod, CompactionOptions};
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let focus = args.trim();
    let options = CompactionOptions {
        method: Some(CompactionMethod::Handoff),
        instructions: (!focus.is_empty()).then(|| focus.to_owned()),
        require_method: true,
        ..CompactionOptions::default()
    };
    super::compaction::run(shell, app, agent, options)
}
