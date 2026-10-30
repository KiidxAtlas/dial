//! Foreground application launcher; monitoring never owns or controls this process.
use crate::config::paths::Layout;
#[cfg(unix)]
use crate::platform::process;
#[cfg(unix)]
use crate::storage::process_capture::Capture;
use anyhow::Result;
#[cfg(unix)]
use anyhow::{Context, ensure};
use std::ffi::OsString;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::{
    io::{self, Read, Write},
    process::{Command, Stdio},
};

#[derive(Clone, Debug, clap::Args)]
pub struct RunArgs {
    /// Project-local name offered to approved monitor start calls in other Dial sessions.
    #[arg(long, default_value = "app")]
    pub name: String,
    /// Executable and arguments (no shell expansion). Use -- before the command.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[cfg(unix)]
pub fn run(layout: &Layout, args: &RunArgs) -> Result<i32> {
    ensure!(
        !args.command.is_empty(),
        "run requires an executable after --"
    );
    let mut capture = Capture::start(layout, &args.name)?;
    let mut command = Command::new(&args.command[0]);
    command
        .args(&args.command[1..])
        .current_dir(&layout.project_root);
    let result = unix::execute(
        &mut command,
        &mut capture,
        io::stdin().is_terminal() && io::stdout().is_terminal(),
    );
    let finish = capture.finish(
        result
            .as_ref()
            .err()
            .map(|_| "foreground wrapper I/O or process failure; see wrapper terminal"),
    );
    match (result, finish) {
        (Ok(code), Ok(())) => Ok(code),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error.context("publishing run completion; capture is incomplete")),
    }
}

/// Windows is refused before creating capture state or launching a child. A blocking pipe
/// fallback cannot safely bound descendant output lifetime; a job-aware relay is required.
#[cfg(windows)]
pub fn run(_layout: &Layout, _args: &RunArgs) -> Result<i32> {
    anyhow::bail!(
        "dial run is currently supported only on Unix; on Windows use an application log file with monitor path"
    )
}

#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
