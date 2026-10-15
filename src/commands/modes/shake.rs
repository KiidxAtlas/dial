//! `/shake [elide|images|thinking]`: drop heavy content from context.
//! `elide` runs Dial's `Shake` compaction method (archived tool results are replaced by artifact references).
//! `images` and `thinking` have no engine path in Dial and are reported as unsupported.

use crate::agent::Agent;
use crate::agent::compaction::{CompactionMethod, CompactionOptions};
use crate::app::{App, Shell, Step};
use anyhow::{Result, bail};

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let mode = args.trim();
    let mode = if mode.is_empty() { "elide" } else { mode };
    match mode {
        "elide" => {
            let options = CompactionOptions {
                method: Some(CompactionMethod::Shake),
                require_method: true,
                ..CompactionOptions::default()
            };
            super::compaction::run(shell, app, agent, options)
        }
        "images" | "thinking" => bail!(
            "/shake {mode} is not supported: Dial's shake engine only elides archived tool results (use /shake elide)"
        ),
        other => bail!("unknown shake mode `{other}`; use elide, images, or thinking"),
    }
}
