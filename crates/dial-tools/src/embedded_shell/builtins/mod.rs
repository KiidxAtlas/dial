//! Coreutils-style commands for the built-in shell. Most commands are synchronous functions over
//! [`Io`]; one shared adapter ([`execute`]) dispatches by command name, so every command is
//! registered with the same brush `Registration`. Commands that run other commands (`xargs`,
//! `find -exec`) are async and take the brush [`ExecutionContext`] so that they start commands
//! through the shell engine ([`commands`]). On Windows, `sleep` is async so an aborted run cancels
//! it; Unix keeps the system `sleep` because a backgrounded builtin starts no process, so
//! `sleep N & kill $!` would have no PID to signal. `kill` and `wait` replace brush's on every OS
//! so that `kill` sends `TERM` by default and `wait $!` returns the job's status ([`system`],
//! [`wait`]). Exit codes: 0 success, 1 failure, 2 usage error.
//!
//! Each bundled command implements a subset of its options. Arguments it cannot parse are
//! declined ([`Io::decline`]) before any input is read or output written, and [`execute`] then
//! runs the program of the same name from `PATH` with the unchanged arguments. Only when `PATH`
//! has no such program is the usage error reported, so `grep -P` or `sed -i` keep working
//! wherever the system provides them.

mod commands;
mod files;
mod stream;
mod system;
mod text;
mod wait;

use std::fmt::Display;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use brush_core::builtins::{ContentOptions, ContentType, Registration};
use brush_core::extensions::DefaultShellExtensions;
use brush_core::{CommandArg, Error, ErrorKind, ExecutionContext, ExecutionResult};
use futures::future::BoxFuture;

/// Standard streams and environment for one command invocation.
pub(crate) struct Io<'a> {
    /// Name the command was invoked as, used as the prefix of error messages.
    pub name: &'a str,
    /// Shell working directory; relative paths resolve against it.
    pub cwd: &'a Path,
    /// Exported shell variables, for commands that spawn programs.
    pub env: &'a [(String, String)],
    /// One-line usage, printed for `--help` and usage errors.
    pub synopsis: &'a str,
    pub stdin: &'a mut (dyn Read + Send),
    pub stdout: &'a mut (dyn Write + Send),
    pub stderr: &'a mut (dyn Write + Send),
    /// Set by [`Io::decline`]: why the arguments are outside this command's supported subset.
    pub declined: Option<String>,
}

impl Io<'_> {
    /// Resolves an argument against the shell working directory.
    pub fn path(&self, arg: &str) -> PathBuf {
        let path = Path::new(arg);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Writes `name: message` to stderr and returns exit code 1.
    pub fn fail(&mut self, message: impl Display) -> i32 {
        let _ = writeln!(self.stderr, "{}: {message}", self.name);
        1
    }

    /// Writes the usage line to stderr and returns exit code 2.
    pub fn usage(&mut self) -> i32 {
        let _ = writeln!(self.stderr, "usage: {}", self.synopsis);
        2
    }

    /// Declines arguments this command does not support (an unknown option or an unparsable
    /// argument), before any input is read or output written. Nothing is printed here: the
    /// dispatcher runs the `PATH` program of the same name instead, or reports `message` and the
    /// usage line when there is none. Returns exit code 2.
    pub fn decline(&mut self, message: impl Into<String>) -> i32 {
        self.declined = Some(message.into());
        2
    }
}

/// Reports a bad option or argument, then the usage line. Returns exit code 2.
pub(crate) fn usage_error(io: &mut Io<'_>, message: &str) -> i32 {
    let _ = writeln!(io.stderr, "{}: {message}", io.name);
    io.usage()
}

/// Signature shared by every synchronous command. `args` excludes the command name.
pub(crate) type RunFn = fn(&mut Io<'_>, &[String]) -> i32;

/// How a command runs. Shell-engine commands are dispatched by variant because they are async
/// and need the execution context, which a plain fn pointer cannot carry.
pub(crate) enum Runner {
    /// Runs to completion over [`Io`] without starting other commands.
    Sync(RunFn),
    /// `xargs`: starts commands through the shell engine.
    Xargs,
    /// `find`: with `-exec`, starts commands through the shell engine.
    Find,
    /// `sleep` on Windows: awaits a timer, so cancelling the run cancels the wait.
    #[cfg(windows)]
    Sleep,
    /// `kill`: Dial's on every OS (brush's is Unix-only and defaults to `KILL`).
    Kill,
    /// `wait`: awaits jobs in the shell's job table.
    Wait,
}

pub(crate) struct Util {
    pub name: &'static str,
    pub synopsis: &'static str,
    pub run: Runner,
}

pub(crate) const UTILS: &[Util] = &[
    Util {
        name: "kill",
        synopsis: "kill [-s SIG | -SIG | -n NUM] PID|%JOB... | kill -l [SIG|NUM]...",
        run: Runner::Kill,
    },
    Util {
        name: "basename",
        synopsis: "basename NAME [SUFFIX]",
        run: Runner::Sync(files::basename),
    },
    Util {
        name: "cat",
        synopsis: "cat [-n] [FILE]...",
        run: Runner::Sync(files::cat),
    },
    Util {
        name: "cp",
        synopsis: "cp [-rRf] SOURCE... DEST",
        run: Runner::Sync(files::cp),
    },
    Util {
        name: "dirname",
        synopsis: "dirname NAME...",
        run: Runner::Sync(files::dirname),
    },
    Util {
        name: "find",
        synopsis: "find [PATH] [-name GLOB] [-type f|d] [-maxdepth N] [-print|-print0] [-exec CMD... ; | -exec CMD... {} +]",
        run: Runner::Find,
    },
    Util {
        name: "head",
        synopsis: "head [-n N | -c N] [FILE]...",
        run: Runner::Sync(files::head),
    },
    Util {
        name: "ls",
        synopsis: "ls [-a] [-l] [PATH]...",
        run: Runner::Sync(files::ls),
    },
    Util {
        name: "mkdir",
        synopsis: "mkdir [-p] DIR...",
        run: Runner::Sync(files::mkdir),
    },
    Util {
        name: "mktemp",
        synopsis: "mktemp [-d] [-p DIR] [TEMPLATE]",
        run: Runner::Sync(system::mktemp),
    },
    Util {
        name: "mv",
        synopsis: "mv [-f] SOURCE... DEST",
        run: Runner::Sync(files::mv),
    },
    Util {
        name: "realpath",
        synopsis: "realpath [-e|-m] PATH...",
        run: Runner::Sync(system::realpath),
    },
    Util {
        name: "rm",
        synopsis: "rm [-rRf] PATH...",
        run: Runner::Sync(files::rm),
    },
    Util {
        name: "seq",
        synopsis: "seq [-w] [-s SEP] [FIRST [INCR]] LAST",
        run: Runner::Sync(stream::seq),
    },
    #[cfg(windows)]
    Util {
        name: "sleep",
        synopsis: "sleep NUMBER[smhd]...",
        run: Runner::Sleep,
    },
    Util {
        name: "tail",
        synopsis: "tail [-n [+]N] [FILE]...",
        run: Runner::Sync(files::tail),
    },
    Util {
        name: "tee",
        synopsis: "tee [-a] [FILE]...",
        run: Runner::Sync(stream::tee),
    },
    Util {
        name: "touch",
        synopsis: "touch FILE...",
        run: Runner::Sync(files::touch),
    },
    Util {
        name: "wc",
        synopsis: "wc [-l] [-w] [-c] [FILE]...",
        run: Runner::Sync(files::wc),
    },
    Util {
        name: "cut",
        synopsis: "cut -d DELIM -f LIST [FILE]...",
        run: Runner::Sync(text::cut),
    },
    Util {
        name: "env",
        synopsis: "env",
        run: Runner::Sync(text::env),
    },
    Util {
        name: "grep",
        synopsis: "grep [-EFivnclrqhHoxws] [-e PATTERN]... [PATTERN] [FILE]...",
        run: Runner::Sync(text::grep),
    },
    Util {
        name: "sed",
        synopsis: "sed [-n] [-e SCRIPT]... [FILE]...",
        run: Runner::Sync(text::sed),
    },
    Util {
        name: "sort",
        synopsis: "sort [-r] [-n] [-u] [FILE]...",
        run: Runner::Sync(text::sort),
    },
    Util {
        name: "tr",
        synopsis: "tr [-d] SET1 [SET2]",
        run: Runner::Sync(text::tr),
    },
    Util {
        name: "uniq",
        synopsis: "uniq [-c] [-d] [-u] [FILE]",
        run: Runner::Sync(text::uniq),
    },
    Util {
        name: "which",
        synopsis: "which [-a] NAME...",
        run: Runner::Sync(system::which),
    },
    Util {
        name: "wait",
        synopsis: "wait [%JOB | PID]...",
        run: Runner::Wait,
    },
    Util {
        name: "xargs",
        synopsis: "xargs [-0] [-r] [-n N] [-I REPL] [COMMAND [ARG]...]",
        run: Runner::Xargs,
    },
];

/// Registrations for every command, keyed by name, for `ShellBuilder::builtins`.
pub(crate) fn registrations()
-> std::collections::HashMap<String, Registration<DefaultShellExtensions>> {
    UTILS
        .iter()
        .map(|util| {
            (
                util.name.to_owned(),
                Registration {
                    execute_func: execute,
                    content_func: content,
                    disabled: false,
                    special_builtin: false,
                    declaration_builtin: false,
                },
            )
        })
        .collect()
}

fn util_named(name: &str) -> Option<&'static Util> {
    UTILS.iter().find(|util| util.name == name)
}

fn execute(
    mut context: ExecutionContext<'_>,
    args: Vec<CommandArg>,
) -> BoxFuture<'_, Result<ExecutionResult, Error>> {
    Box::pin(async move {
        let Some(util) = util_named(&context.command_name) else {
            return Ok(ExecutionResult::new(127));
        };
        // Brush passes the command name as args[0].
        let argv: Vec<String> = args.iter().skip(1).map(ToString::to_string).collect();
        let cwd = context.shell.working_dir().to_path_buf();
        let env: Vec<(String, String)> = context
            .shell
            .env()
            .iter_exported()
            .map(|(name, var)| {
                (
                    name.clone(),
                    var.value().to_cow_str(context.shell).into_owned(),
                )
            })
            .collect();
        let mut stdin = context.stdin();
        let mut stdout = context.stdout();
        let mut stderr = context.stderr();
        let mut io = Io {
            name: util.name,
            cwd: &cwd,
            env: &env,
            synopsis: util.synopsis,
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
            declined: None,
        };
        let code = if argv.first().map(String::as_str) == Some("--help") {
            let _ = writeln!(io.stdout, "usage: {}", util.synopsis);
            0
        } else {
            match util.run {
                Runner::Sync(run) => run(&mut io, &argv),
                Runner::Xargs => commands::xargs(&mut io, &mut context, &argv).await,
                Runner::Find => commands::find(&mut io, &mut context, &argv).await,
                Runner::Wait => wait::wait(&mut io, &mut context, &argv).await,
                #[cfg(windows)]
                Runner::Sleep => system::sleep(&mut io, &argv).await,
                Runner::Kill => system::kill(&mut io, &mut context, &argv).await,
            }
        };
        let code = match io.declined.take() {
            Some(message) => run_declined(&mut io, &mut context, &argv, &message).await,
            None => code,
        };
        Ok(ExecutionResult::new(u8::try_from(code).unwrap_or(1)))
    })
}

/// Runs the `PATH` program named like the bundled command with the arguments it declined, or
/// reports `message` and the usage line (exit 2) when `PATH` has no such program.
async fn run_declined(
    io: &mut Io<'_>,
    context: &mut ExecutionContext<'_>,
    argv: &[String],
    message: &str,
) -> i32 {
    let Some(program) = context.shell.find_first_executable_in_path(io.name) else {
        return usage_error(io, message);
    };
    // A path, so that resolution skips the bundled builtin of the same name.
    let command: Vec<String> = std::iter::once(program.display().to_string())
        .chain(argv.iter().cloned())
        .collect();
    match commands::invoke(context, &command, commands::Stdin::Inherit).await {
        Ok(commands::Outcome::Exited(code)) => i32::from(code),
        // A stopped program reports SIGTSTP's status, as the shell does for a stopped job.
        Ok(commands::Outcome::Stopped) => 128 + 20,
        Err(error) => {
            commands::report_failure(io, &command[0], &error);
            commands::failure_status(&error)
        }
    }
}

fn content(
    name: &str,
    _content_type: ContentType,
    _options: &ContentOptions,
) -> Result<String, Error> {
    match util_named(name) {
        Some(util) => Ok(format!("{}\n", util.synopsis)),
        None => Err(ErrorKind::Unimplemented("no such embedded builtin").into()),
    }
}

#[cfg(test)]
mod tests;
