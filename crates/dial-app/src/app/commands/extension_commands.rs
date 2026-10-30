//! Slash commands a loaded runtime extension declared at its handshake. Built-in groups run first,
//! so an extension can never shadow a built-in name. The extension's reply runs through
//! `crate::extensions::command`, which drives panels via the interactive `Ui` bridge.
use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY, Shell, Step};
use anyhow::Result;
use std::time::Instant;

/// Runs `name` when a loaded extension declared it; `None` leaves the name to the next group.
pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    let host = agent.extension_host()?;
    let plugin = host.command_owner(name)?;
    let result = {
        let mut bridge = Bridge {
            app,
            shell: &mut *shell,
            interrupt: app.interrupt.clone(),
            started: Instant::now(),
            last_paint: Instant::now() - PAINT_EVERY,
            failed: false,
            pending: Vec::new(),
            next_id: 0,
        };
        bridge.repaint(true);
        agent.run_extension_command(&mut bridge, &host, &plugin, name, args.trim())
    };
    Some(result.map(|text| {
        if !text.is_empty() {
            shell.note(name, text);
        }
        Step::Done
    }))
}
