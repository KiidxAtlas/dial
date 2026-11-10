//! Foreground application launcher; monitoring never owns or controls this process.
use crate::config::paths::Layout;
use crate::platform::process;
use crate::storage::process_capture::Capture;
use anyhow::Result;
use anyhow::{Context, ensure};
use std::ffi::OsString;
#[cfg(unix)]
use std::io::IsTerminal;
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
    #[cfg(unix)]
    let result = unix::execute(
        &mut command,
        &mut capture,
        io::stdin().is_terminal() && io::stdout().is_terminal(),
    );
    #[cfg(windows)]
    let result = windows::execute(&mut command, &mut capture);
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

#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
