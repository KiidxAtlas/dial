//! `wait` for the embedded shell. Brush's own `wait` rejects process IDs (`wait $!`) and
//! discards the job's status; this one resolves `%JOB` and `$!` PIDs through the shell's job
//! table ([`brush_core::jobs::JobManager::wait_for`]) and returns the waited job's exit status.
//!
//! `-n` waits for the next of the named jobs (every job when none is named) to finish and returns
//! its status, or 127 when there is no such job. `-p VAR` stores the PID of the job waited for in
//! `VAR`. `-f` is accepted: a wait always runs until the job terminates, never stopping at a stop.

use brush_core::ExecutionContext;
use brush_core::env::{EnvironmentLookup, EnvironmentScope};

use super::{Io, usage_error};

/// Exit status when an operand names no job of this shell (bash uses 127).
const NOT_A_CHILD: i32 = 127;

/// The options of one `wait` invocation.
#[derive(Default)]
struct Options {
    /// `-n`: wait for the next job to finish.
    next: bool,
    /// `-p VAR`: the variable that receives the PID of the job waited for.
    print_into: Option<String>,
}

/// Splits the options from the operands. `Err` carries the exit status to return.
fn parse<'a>(io: &mut Io<'_>, args: &'a [String]) -> Result<(Options, &'a [String]), i32> {
    let mut options = Options::default();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            index += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            break;
        };
        index += 1;
        for (at, flag) in flags.char_indices() {
            match flag {
                'n' => options.next = true,
                'f' => {}
                'p' => {
                    let name = match flags.get(at + 1..).filter(|rest| !rest.is_empty()) {
                        Some(attached) => attached,
                        None => {
                            let Some(next) = args.get(index) else {
                                return Err(usage_error(io, "-p: option requires an argument"));
                            };
                            index += 1;
                            next.as_str()
                        }
                    };
                    if !brush_core::env::valid_variable_name(name) {
                        return Err(io.fail(format!("'{name}': not a valid identifier")));
                    }
                    options.print_into = Some(name.to_owned());
                    break;
                }
                _ => return Err(usage_error(io, &format!("-{flag}: invalid option"))),
            }
        }
    }
    Ok((options, &args[index..]))
}

/// Whether `operand` is a process ID rather than a `%JOB` spec.
fn is_pid(operand: &str) -> bool {
    !operand.is_empty() && operand.bytes().all(|b| b.is_ascii_digit())
}

/// Stores `pid` in the `-p` variable, when one was named and the job's PID is known.
fn store_pid(
    context: &mut ExecutionContext<'_>,
    name: Option<&str>,
    pid: Option<i32>,
) -> Result<(), brush_core::Error> {
    let (Some(name), Some(pid)) = (name, pid) else {
        return Ok(());
    };
    context.shell.env_mut().update_or_add(
        name,
        pid.to_string().into(),
        |_| Ok(()),
        EnvironmentLookup::Anywhere,
        EnvironmentScope::Global,
    )
}

/// `wait [-fn] [-p VAR] [ID]...`: with no operands waits for every job and returns 0; otherwise
/// waits for each `%JOB` or PID in turn and returns the status of the last one. With `-n` it waits
/// for the next of them to finish instead.
pub(super) async fn wait(
    io: &mut Io<'_>,
    context: &mut ExecutionContext<'_>,
    args: &[String],
) -> i32 {
    let (options, ids) = match parse(io, args) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    if let Some(id) = ids.iter().find(|id| !is_pid(id) && !id.starts_with('%')) {
        return usage_error(io, &format!("'{id}': not a pid or valid job spec"));
    }
    if options.next {
        return wait_next(io, context, &options, ids).await;
    }
    if ids.is_empty() {
        return match context.shell.jobs_mut().wait_all().await {
            Ok(_) => 0,
            Err(error) => io.fail(error),
        };
    }
    let mut status = 0;
    for id in ids {
        let pid = context.shell.jobs_mut().job_pid(id);
        status = match context.shell.jobs_mut().wait_for(id).await {
            Some(Ok(result)) => match store_pid(context, options.print_into.as_deref(), pid) {
                Ok(()) => i32::from(u8::from(&result.exit_code)),
                Err(error) => io.fail(error),
            },
            Some(Err(error)) => io.fail(error),
            None if is_pid(id) => {
                io.fail(format!("pid {id} is not a child of this shell"));
                NOT_A_CHILD
            }
            None => {
                io.fail(format!("{id}: no such job"));
                NOT_A_CHILD
            }
        };
    }
    status
}

/// `wait -n [-p VAR] [ID]...`: waits for the next of the named jobs (every job when none is named)
/// to finish and returns its status.
async fn wait_next(
    io: &mut Io<'_>,
    context: &mut ExecutionContext<'_>,
    options: &Options,
    ids: &[String],
) -> i32 {
    let Some(outcome) = context.shell.jobs_mut().wait_next(ids).await else {
        return NOT_A_CHILD;
    };
    match outcome {
        Ok((pid, result)) => match store_pid(context, options.print_into.as_deref(), pid) {
            Ok(()) => i32::from(u8::from(&result.exit_code)),
            Err(error) => io.fail(error),
        },
        Err(error) => io.fail(error),
    }
}

#[cfg(test)]
mod tests;
