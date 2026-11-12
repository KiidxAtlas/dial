//! Terminal adapter for the frontend-neutral rewind owner (`crate::rewind`).

use crate::agent::Agent;
use crate::app::App;
use anyhow::Result;

pub(in crate::app) use crate::rewind::Rewound;

/// Drops `user_turns` user turns from the session, preserving the abandoned branch as a summary;
/// the summary is interruptible through the app interrupt flag.
pub(in crate::app) fn rewind(
    app: &App,
    agent: &mut Agent<'_>,
    user_turns: usize,
) -> Result<Rewound> {
    crate::rewind::rewind(&app.config, &app.interrupt, agent, user_turns)
}
