//! Commands that run other commands: `xargs` and `find -exec`.
//!
//! They never build shell text. Each invocation is an argv handed to brush's `SimpleCommand`,
//! which resolves the name the way a typed command is resolved (shell function, then bundled
//! builtin, then `PATH`) and passes the arguments as values. Each invocation runs on a copy of the
//! shell, as a subprocess would: its changes to variables, functions and the working directory are
//! discarded. Redirections of the calling command (stdout, stderr, and stdin for `find`) are kept.

mod find;
mod xargs;

use std::fs::File;

use brush_core::commands::{CommandArg, ShellForCommand, SimpleCommand};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::results::ExecutionWaitResult;
use brush_core::{Error, ErrorKind, ExecutionContext};

use super::Io;

pub(super) use find::find;
pub(super) use xargs::xargs;

/// Where a started command reads its standard input.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Stdin {
    /// The calling command's standard input (`find -exec`).
    Inherit,
    /// The null device (`xargs`, as GNU xargs does, so commands cannot consume the input list).
    Null,
}

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// How a started command ended. A stop is kept apart from exit codes: a command that exits 125
/// itself is an ordinary failure, while a stopped run ends the batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Ran to completion with this exit code.
    Exited(u8),
    /// Stopped before completing.
    Stopped,
}

/// Runs `argv` (command name first) through the shell engine.
///
/// `Err` means the command could not be started (not found, not executable, I/O failure); an
/// exit code, including one from a failing function or builtin, is `Ok(Outcome::Exited(_))`.
pub(super) async fn invoke(
    ctx: &mut ExecutionContext<'_>,
    argv: &[String],
    stdin: Stdin,
) -> Result<Outcome, Error> {
    let Some(name) = argv.first() else {
        return Ok(Outcome::Exited(0));
    };
    let mut params = ctx.params.clone();
    if stdin == Stdin::Null {
        let null = File::open(NULL_DEVICE).map_err(ErrorKind::IoError)?;
        params.set_fd(OpenFiles::STDIN_FD, OpenFile::File(null));
    }
    let args: Vec<CommandArg> = argv
        .iter()
        .map(|arg| CommandArg::String(arg.clone()))
        .collect();
    let command = SimpleCommand::new(
        ShellForCommand::OwnedShell {
            target: Box::new(ctx.shell.clone()),
            parent: &mut *ctx.shell,
        },
        params,
        name.clone(),
        args,
    );
    let started = command.execute().await?;
    match started.wait().await? {
        ExecutionWaitResult::Completed(result) => Ok(Outcome::Exited(u8::from(&result.exit_code))),
        ExecutionWaitResult::Stopped(_) => Ok(Outcome::Stopped),
    }
}

/// Exit status for a command that could not be started, matching the shell: 127 when the name
/// does not resolve, 126 when it resolves but cannot run.
pub(super) fn failure_status(error: &Error) -> i32 {
    match error.kind() {
        ErrorKind::CommandNotFound(_) => 127,
        _ => 126,
    }
}

/// Writes `name: reason` for a command that could not be started to stderr.
pub(super) fn report_failure(io: &mut Io<'_>, name: &str, error: &Error) {
    let reason = match error.kind() {
        ErrorKind::CommandNotFound(_) => "command not found".to_owned(),
        _ => error.to_string(),
    };
    let _ = writeln!(io.stderr, "{}: {name}: {reason}", io.name);
}

#[cfg(test)]
mod tests;
