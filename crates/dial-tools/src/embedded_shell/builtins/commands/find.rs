//! `find` with the `-name`, `-type` and `-maxdepth` tests and the `-print`, `-print0` and `-exec`
//! actions. `-exec` starts commands through the shell engine (see [`super::invoke`]).
//!
//! Each test may appear once and must come before the actions, so the tests apply to every match
//! as a conjunction. The actions run for each match in command-line order, after the whole tree
//! has been walked.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;

use brush_core::ExecutionContext;

use super::super::Io;
use super::super::files::{base_name, err_text, glob_match};
use super::{Outcome, Stdin, invoke, report_failure};

/// Most paths handed to one `-exec ... {} +` command.
const BATCH_MAX_PATHS: usize = 1000;

/// Budget for the encoded length of one command line, in the units of [`arg_cost`]. Windows
/// limits a command line to 32767 UTF-16 units, so its budget stays well below that. Elsewhere
/// the budget keeps batches far below typical ARG_MAX limits.
fn command_line_budget(windows: bool) -> usize {
    if windows { 24_000 } else { 64 * 1024 }
}

/// Cost of one argument in an encoded command line. On Windows that is its UTF-16 length, doubled
/// to allow for escaping, plus room for the quotes and the separator. Elsewhere it is its byte
/// length plus the separator.
fn arg_cost(arg: &str, windows: bool) -> usize {
    if windows {
        arg.encode_utf16().count() * 2 + 3
    } else {
        arg.len() + 1
    }
}

/// Cost of a whole argument vector, command name included.
fn argv_cost(argv: &[String], windows: bool) -> usize {
    argv.iter().map(|arg| arg_cost(arg, windows)).sum()
}

#[derive(Debug, PartialEq)]
enum Action {
    Print,
    Print0,
    /// `-exec CMD ;` runs CMD once per path, with `{}` replaced by the path. `-exec CMD {} +` runs
    /// CMD with the paths appended, in as few invocations as the batch limits allow.
    Exec {
        argv: Vec<String>,
        batched: bool,
    },
}

/// A parsed `find` command line.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    root: String,
    name: Option<String>,
    kind: Option<char>,
    max_depth: Option<usize>,
    /// Actions in command-line order. Without any, matches are printed.
    actions: Vec<Action>,
}

impl Plan {
    fn parse(args: &[String]) -> Result<Plan, String> {
        let (root, rest) = match args.first() {
            Some(first)
                if !first.starts_with('-') && !matches!(first.as_str(), "(" | ")" | "!") =>
            {
                (first.clone(), &args[1..])
            }
            _ => (".".to_owned(), args),
        };
        let mut plan = Plan {
            root,
            ..Plan::default()
        };
        let mut i = 0;
        while i < rest.len() {
            let flag = rest[i].as_str();
            match flag {
                "-print" => {
                    plan.actions.push(Action::Print);
                    i += 1;
                }
                "-print0" => {
                    plan.actions.push(Action::Print0);
                    i += 1;
                }
                "-exec" => {
                    let (action, next) = exec_action(rest, i + 1)?;
                    plan.actions.push(action);
                    i = next;
                }
                "-name" | "-type" | "-maxdepth" => {
                    if !plan.actions.is_empty() {
                        return Err(format!("'{flag}' must come before the actions"));
                    }
                    let value = rest
                        .get(i + 1)
                        .map(String::as_str)
                        .ok_or_else(|| format!("missing argument to '{flag}'"))?;
                    let duplicate = || format!("'{flag}' may be given only once");
                    match flag {
                        "-name" => {
                            if plan.name.is_some() {
                                return Err(duplicate());
                            }
                            plan.name = Some(value.to_owned());
                        }
                        "-type" => {
                            if plan.kind.is_some() {
                                return Err(duplicate());
                            }
                            plan.kind = Some(match value {
                                "f" => 'f',
                                "d" => 'd',
                                _ => return Err(format!("unsupported -type '{value}'")),
                            });
                        }
                        _ => {
                            if plan.max_depth.is_some() {
                                return Err(duplicate());
                            }
                            plan.max_depth = Some(
                                value
                                    .parse()
                                    .map_err(|_| format!("invalid -maxdepth '{value}'"))?,
                            );
                        }
                    }
                    i += 2;
                }
                _ => return Err(format!("unknown predicate '{flag}'")),
            }
        }
        if plan.actions.is_empty() {
            plan.actions.push(Action::Print);
        }
        Ok(plan)
    }

    fn matches(&self, meta: &fs::Metadata, shown: &str) -> bool {
        let kind_ok = match self.kind {
            Some('f') => meta.is_file(),
            Some(_) => meta.is_dir(),
            None => true,
        };
        let name_ok = self
            .name
            .as_deref()
            .is_none_or(|glob| glob_match(glob, base_name(shown)));
        kind_ok && name_ok
    }
}

/// Parses the command of `-exec` starting at `start`, which ends at `;`, or at `+` directly after
/// the final `{}`. Returns the action and the index after the terminator.
fn exec_action(rest: &[String], start: usize) -> Result<(Action, usize), String> {
    for end in start..rest.len() {
        let (argv_end, batched, next) = match rest[end].as_str() {
            ";" => (end, false, end + 1),
            "+" if end > start && rest[end - 1] == "{}" => (end - 1, true, end + 1),
            _ => continue,
        };
        let argv = rest[start..argv_end].to_vec();
        if argv.is_empty() {
            return Err("-exec needs a command".to_owned());
        }
        // GNU find substitutes `{}` only as the final argument of the `+` form.
        if batched && argv.iter().any(|arg| arg.contains("{}")) {
            return Err("'{}' may only be the final argument of -exec ... {} +".to_owned());
        }
        return Ok((Action::Exec { argv, batched }, next));
    }
    Err("-exec needs a terminating ';' or '{} +'".to_owned())
}

/// Joins a displayed parent path and a child name with a single `/`.
fn join_display(parent: &str, child: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

/// Depth-first walk from the root that collects displayed paths of matches. Children are visited
/// in name order and symlinks are not followed. Problems are reported to `err` and make the
/// status 1. An entry whose name is not UTF-8 is skipped, because its displayed path could not
/// name the file that a command would act on.
struct Walk<'a> {
    prog: &'a str,
    plan: &'a Plan,
    hits: Vec<String>,
}

impl Walk<'_> {
    fn visit(&mut self, err: &mut dyn Write, path: &Path, shown: &str, depth: usize) -> i32 {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                let _ = writeln!(err, "{}: '{shown}': {}", self.prog, err_text(&e));
                return 1;
            }
        };
        if self.plan.matches(&meta, shown) {
            self.hits.push(shown.to_owned());
        }
        let mut status = 0;
        if meta.is_dir() && self.plan.max_depth.is_none_or(|max| depth < max) {
            match fs::read_dir(path) {
                Ok(entries) => {
                    let mut names: Vec<OsString> = entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name())
                        .collect();
                    names.sort();
                    for child in names {
                        let Some(child_name) = child.to_str() else {
                            let _ = writeln!(
                                err,
                                "{}: '{shown}': entry name is not valid UTF-8; skipped",
                                self.prog
                            );
                            status = 1;
                            continue;
                        };
                        let child_shown = join_display(shown, child_name);
                        status |= self.visit(err, &path.join(&child), &child_shown, depth + 1);
                    }
                }
                Err(e) => {
                    let _ = writeln!(err, "{}: '{shown}': {}", self.prog, err_text(&e));
                    status = 1;
                }
            }
        }
        status
    }
}

/// Paths waiting for one `-exec ... {} +` command, with the encoded cost of its command line.
struct Batch {
    template_cost: usize,
    budget: usize,
    paths: Vec<String>,
    cost: usize,
}

impl Batch {
    fn new(template: &[String], windows: bool) -> Batch {
        Batch {
            template_cost: argv_cost(template, windows),
            budget: command_line_budget(windows),
            paths: Vec::new(),
            cost: 0,
        }
    }

    /// Whether a path of `cost` can join the batch without exceeding the path count or the budget.
    fn fits(&self, cost: usize) -> bool {
        self.paths.len() < BATCH_MAX_PATHS && self.template_cost + self.cost + cost <= self.budget
    }

    /// Whether a path of `cost` fits in a command line of its own. If not, it can never run.
    fn fits_alone(&self, cost: usize) -> bool {
        self.template_cost + cost <= self.budget
    }

    fn push(&mut self, path: &str, cost: usize) {
        self.cost += cost;
        self.paths.push(path.to_owned());
    }

    fn take(&mut self) -> Vec<String> {
        self.cost = 0;
        std::mem::take(&mut self.paths)
    }
}

/// Replaces every `{}` in `argv` with the displayed path.
fn substitute(argv: &[String], shown: &str) -> Vec<String> {
    argv.iter().map(|arg| arg.replace("{}", shown)).collect()
}

/// Runs one command for `find` and returns its exit code. Returns `None` after reporting that the
/// command could not be started or was stopped.
async fn run_command(
    io: &mut Io<'_>,
    ctx: &mut ExecutionContext<'_>,
    argv: &[String],
) -> Option<u8> {
    match invoke(ctx, argv, Stdin::Inherit).await {
        Ok(Outcome::Exited(code)) => Some(code),
        Ok(Outcome::Stopped) => {
            let _ = writeln!(io.stderr, "{}: {}: stopped", io.name, argv[0]);
            None
        }
        Err(error) => {
            report_failure(io, &argv[0], &error);
            None
        }
    }
}

/// Runs the pending batch of one `-exec ... {} +` action, if it holds any path. As in GNU find,
/// the batch makes find's status 1 when its command fails.
async fn flush_batch(
    io: &mut Io<'_>,
    ctx: &mut ExecutionContext<'_>,
    template: &[String],
    batch: &mut Batch,
) -> i32 {
    if batch.paths.is_empty() {
        return 0;
    }
    let argv: Vec<String> = template.iter().cloned().chain(batch.take()).collect();
    match run_command(io, ctx, &argv).await {
        Some(0) => 0,
        _ => 1,
    }
}

/// `find [PATH] [-name GLOB] [-type f|d] [-maxdepth N] [-print|-print0|-exec ...]...`.
pub(in super::super) async fn find(
    io: &mut Io<'_>,
    ctx: &mut ExecutionContext<'_>,
    args: &[String],
) -> i32 {
    let plan = match Plan::parse(args) {
        Ok(plan) => plan,
        Err(message) => return io.decline(message),
    };
    let windows = cfg!(windows);
    let budget = command_line_budget(windows);
    let root = io.path(&plan.root);
    let mut walk = Walk {
        prog: io.name,
        plan: &plan,
        hits: Vec::new(),
    };
    let mut status = walk.visit(&mut *io.stderr, &root, &plan.root, 0);
    let hits = walk.hits;
    let mut batches: Vec<Batch> = plan
        .actions
        .iter()
        .map(|action| match action {
            Action::Exec {
                argv,
                batched: true,
            } => Batch::new(argv, windows),
            _ => Batch::new(&[], windows),
        })
        .collect();

    for shown in &hits {
        for (index, action) in plan.actions.iter().enumerate() {
            match action {
                Action::Print => {
                    let _ = writeln!(io.stdout, "{shown}");
                }
                Action::Print0 => {
                    let _ = io.stdout.write_all(shown.as_bytes());
                    let _ = io.stdout.write_all(&[0]);
                }
                Action::Exec {
                    argv,
                    batched: false,
                } => {
                    let command = substitute(argv, shown);
                    if argv_cost(&command, windows) > budget {
                        let _ =
                            writeln!(io.stderr, "{}: '{shown}': argument list too long", io.name);
                        status |= 1;
                        continue;
                    }
                    // `;` runs the command once per path; its exit code only selects the match.
                    if run_command(io, ctx, &command).await.is_none() {
                        status |= 1;
                    }
                }
                Action::Exec {
                    argv,
                    batched: true,
                } => {
                    let cost = arg_cost(shown, windows);
                    let batch = &mut batches[index];
                    if !batch.fits_alone(cost) {
                        let _ =
                            writeln!(io.stderr, "{}: '{shown}': argument list too long", io.name);
                        status |= 1;
                        continue;
                    }
                    if !batch.fits(cost) {
                        status |= flush_batch(io, ctx, argv, batch).await;
                    }
                    batch.push(shown, cost);
                }
            }
        }
    }
    for (index, action) in plan.actions.iter().enumerate() {
        if let Action::Exec {
            argv,
            batched: true,
        } = action
        {
            status |= flush_batch(io, ctx, argv, &mut batches[index]).await;
        }
    }
    status
}

#[cfg(test)]
mod tests;
