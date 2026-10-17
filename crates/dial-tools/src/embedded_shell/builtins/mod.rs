//! Coreutils-style commands for the built-in shell. Each command is a synchronous function over
//! [`Io`]; one shared adapter ([`execute`]) dispatches by command name, so every command is
//! registered with the same brush `Registration`. Exit codes: 0 success, 1 failure, 2 usage error.

mod files;
mod text;

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
    pub stdin: &'a mut dyn Read,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
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
}

/// Signature shared by every command. `args` excludes the command name.
pub(crate) type RunFn = fn(&mut Io<'_>, &[String]) -> i32;

pub(crate) struct Util {
    pub name: &'static str,
    pub synopsis: &'static str,
    pub run: RunFn,
}

pub(crate) const UTILS: &[Util] = &[
    Util {
        name: "basename",
        synopsis: "basename NAME [SUFFIX]",
        run: files::basename,
    },
    Util {
        name: "cat",
        synopsis: "cat [-n] [FILE]...",
        run: files::cat,
    },
    Util {
        name: "cp",
        synopsis: "cp [-r] SOURCE... DEST",
        run: files::cp,
    },
    Util {
        name: "dirname",
        synopsis: "dirname NAME...",
        run: files::dirname,
    },
    Util {
        name: "find",
        synopsis: "find [PATH] [-name GLOB] [-type f|d] [-maxdepth N]",
        run: files::find,
    },
    Util {
        name: "head",
        synopsis: "head [-n N] [FILE]...",
        run: files::head,
    },
    Util {
        name: "ls",
        synopsis: "ls [-a] [-l] [PATH]...",
        run: files::ls,
    },
    Util {
        name: "mkdir",
        synopsis: "mkdir [-p] DIR...",
        run: files::mkdir,
    },
    Util {
        name: "mv",
        synopsis: "mv SOURCE... DEST",
        run: files::mv,
    },
    Util {
        name: "rm",
        synopsis: "rm [-r] [-f] PATH...",
        run: files::rm,
    },
    Util {
        name: "tail",
        synopsis: "tail [-n N] [FILE]...",
        run: files::tail,
    },
    Util {
        name: "touch",
        synopsis: "touch FILE...",
        run: files::touch,
    },
    Util {
        name: "wc",
        synopsis: "wc [-l] [-w] [-c] [FILE]...",
        run: files::wc,
    },
    Util {
        name: "cut",
        synopsis: "cut -d DELIM -f LIST [FILE]...",
        run: text::cut,
    },
    Util {
        name: "env",
        synopsis: "env",
        run: text::env,
    },
    Util {
        name: "grep",
        synopsis: "grep [-i] [-v] [-n] [-c] [-l] [-r] PATTERN [FILE]...",
        run: text::grep,
    },
    Util {
        name: "sed",
        synopsis: "sed [-n] [-e] 's/RE/REPL/[g]' [FILE]...",
        run: text::sed,
    },
    Util {
        name: "sort",
        synopsis: "sort [-r] [-n] [-u] [FILE]...",
        run: text::sort,
    },
    Util {
        name: "tr",
        synopsis: "tr [-d] SET1 [SET2]",
        run: text::tr,
    },
    Util {
        name: "uniq",
        synopsis: "uniq [-c] [-d] [-u] [FILE]",
        run: text::uniq,
    },
    Util {
        name: "xargs",
        synopsis: "xargs [-n N] [-I REPL] COMMAND [ARG]...",
        run: text::xargs,
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
    context: ExecutionContext<'_>,
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
        };
        let code = if argv.first().map(String::as_str) == Some("--help") {
            let _ = writeln!(io.stdout, "usage: {}", util.synopsis);
            0
        } else {
            (util.run)(&mut io, &argv)
        };
        Ok(ExecutionResult::new(u8::try_from(code).unwrap_or(1)))
    })
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
