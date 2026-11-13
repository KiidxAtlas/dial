//! `kill` for the embedded shell on every OS. brush-builtins compiles its `kill` for Unix only,
//! and there it defaults to `KILL` rather than bash's `TERM`, so a backgrounded server never got
//! a chance to shut down cleanly.
//!
//! Unix sends the real signal (`kill(2)`), numbered as the host defines it; `kill -- -PGID` sends
//! it to a process group (`killpg(2)`). Windows has no signals: any signal except 0 terminates the
//! target and its descendants (so a `.cmd` shim's `node.exe` does not outlive it) with exit status
//! 128 plus the signal number, the status a Unix shell reports for a signalled child. Signal 0 only
//! checks that the process exists.
//!
//! A `%JOB`, or a PID that names one of this shell's jobs (`$!`), goes through the job table
//! ([`brush_core::jobs::JobManager::signal_job`]): the signal also reaches every external process
//! the job started, and a signal that ends the job ends its task too, so `wait` reports
//! `128 + signal`. Any other PID is signalled directly.

use brush_core::ExecutionContext;
#[cfg(windows)]
use dial_core::platform::process::{process_exists, terminate_tree};

use super::super::Io;
use super::super::files::err_text;
use super::super::usage_error;

/// Signal names and their numbers on this host, as `kill -l` lists them.
#[cfg(unix)]
#[allow(
    clippy::cast_sign_loss,
    reason = "signal numbers are small positive constants"
)]
const SIGNALS: &[(&str, u32)] = &[
    ("HUP", libc::SIGHUP as u32),
    ("INT", libc::SIGINT as u32),
    ("QUIT", libc::SIGQUIT as u32),
    ("ILL", libc::SIGILL as u32),
    ("TRAP", libc::SIGTRAP as u32),
    ("ABRT", libc::SIGABRT as u32),
    ("BUS", libc::SIGBUS as u32),
    ("FPE", libc::SIGFPE as u32),
    ("KILL", libc::SIGKILL as u32),
    ("USR1", libc::SIGUSR1 as u32),
    ("SEGV", libc::SIGSEGV as u32),
    ("USR2", libc::SIGUSR2 as u32),
    ("PIPE", libc::SIGPIPE as u32),
    ("ALRM", libc::SIGALRM as u32),
    ("TERM", libc::SIGTERM as u32),
    ("CHLD", libc::SIGCHLD as u32),
    ("CONT", libc::SIGCONT as u32),
    ("STOP", libc::SIGSTOP as u32),
    ("TSTP", libc::SIGTSTP as u32),
    ("TTIN", libc::SIGTTIN as u32),
    ("TTOU", libc::SIGTTOU as u32),
];

/// Signal names and their Linux numbers, as `kill -l` lists them.
#[cfg(windows)]
const SIGNALS: &[(&str, u32)] = &[
    ("HUP", 1),
    ("INT", 2),
    ("QUIT", 3),
    ("ILL", 4),
    ("TRAP", 5),
    ("ABRT", 6),
    ("BUS", 7),
    ("FPE", 8),
    ("KILL", 9),
    ("USR1", 10),
    ("SEGV", 11),
    ("USR2", 12),
    ("PIPE", 13),
    ("ALRM", 14),
    ("TERM", 15),
    ("CHLD", 17),
    ("CONT", 18),
    ("STOP", 19),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
];

/// Signal number 15 (`TERM`), the default for `kill`.
const DEFAULT_SIGNAL: u32 = 15;

fn signal_name(number: u32) -> Option<&'static str> {
    SIGNALS
        .iter()
        .find(|(_, known)| *known == number)
        .map(|(name, _)| *name)
}

/// A signal from a number (`9`), a name (`KILL`), or a name with its prefix (`SIGKILL`); case
/// does not matter. Zero is accepted as the existence check.
fn signal_number(text: &str) -> Option<u32> {
    if let Ok(number) = text.parse::<u32>() {
        return (number == 0 || signal_name(number).is_some()).then_some(number);
    }
    let upper = text.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, number)| *number)
}

/// `kill -l [SIG|NUM]...`: lists signal names, or converts each operand between name and number.
fn list(io: &mut Io<'_>, operands: &[String]) -> i32 {
    if operands.is_empty() {
        let names: Vec<&str> = SIGNALS.iter().map(|(name, _)| *name).collect();
        let _ = writeln!(io.stdout, "{}", names.join(" "));
        return 0;
    }
    let mut status = 0;
    for operand in operands {
        let converted = match operand.parse::<u32>() {
            Ok(number) => signal_name(number).map(str::to_owned),
            Err(_) => signal_number(operand).map(|number| number.to_string()),
        };
        match converted {
            Some(text) => {
                let _ = writeln!(io.stdout, "{text}");
            }
            None => status = io.fail(format!("{operand}: invalid signal specification")),
        }
    }
    status
}

/// Checks (signal 0) or signals the process `pid`.
#[cfg(unix)]
fn send(pid: u32, signal: u32) -> std::io::Result<()> {
    let signal = i32::try_from(signal)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid signal"))?;
    dial_core::platform::process::signal_pid(pid, signal)
}

/// Checks (signal 0) or signals the process group `group` (`kill -- -GROUP`).
#[cfg(unix)]
fn send_group(group: u32, signal: u32) -> std::io::Result<()> {
    let signal = i32::try_from(signal)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid signal"))?;
    dial_core::platform::process::signal_group(group, signal)
}

/// Checks (signal 0) or terminates the process `pid` and every descendant of it.
#[cfg(windows)]
fn send(pid: u32, signal: u32) -> std::io::Result<()> {
    if signal == 0 {
        return match process_exists(pid)? {
            true => Ok(()),
            false => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such process",
            )),
        };
    }
    terminate_tree(pid, 128 + signal)
}

/// Windows has no process groups to signal: `kill -- -PID` terminates the tree rooted at `PID`.
#[cfg(windows)]
fn send_group(group: u32, signal: u32) -> std::io::Result<()> {
    send(group, signal)
}

/// Whether `signal` ends a job it is sent to. On Unix, stop, continue and child-status signals reach
/// only the job's processes and the job carries on, as in a shell. Windows has no such signals, so
/// every signal except 0 ends the job.
#[cfg(unix)]
fn ends_job(signal: u32) -> bool {
    const NON_TERMINATING: [i32; 6] = [
        libc::SIGCONT,
        libc::SIGSTOP,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGCHLD,
    ];
    i32::try_from(signal).is_ok_and(|signal| signal != 0 && !NON_TERMINATING.contains(&signal))
}

#[cfg(windows)]
fn ends_job(signal: u32) -> bool {
    signal != 0
}

/// Signals the job that `target` (`%JOB` or a job's PID) names, through the shell's job table.
/// `None` when no job matches.
async fn signal_job_target(
    context: &mut ExecutionContext<'_>,
    target: &str,
    signal: u32,
) -> Option<std::io::Result<()>> {
    let number = i32::try_from(signal).unwrap_or_default();
    context
        .shell
        .jobs_mut()
        .signal_job(target, number, ends_job(signal), &|pid: i32| {
            let pid = u32::try_from(pid).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid process id")
            })?;
            send(pid, signal)
        })
        .await
}

/// Reports a failed send to `label` (a PID, or `-GROUP`) in the words `kill` uses; returns 1.
fn report(io: &mut Io<'_>, label: &str, error: &std::io::Error) -> i32 {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "No such process".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Operation not permitted".to_owned(),
        _ => err_text(error),
    };
    io.fail(format!("({label}) - {reason}"))
}

/// One `kill` operand: a `%JOB`, a process ID, or a process group (`-PGID`).
enum Target<'a> {
    Job(&'a str),
    Pid(u32),
    Group(u32),
}

/// Parses a `kill` operand. A group must be positive: `-0` would name the caller's own group.
fn parse_target(operand: &str) -> Option<Target<'_>> {
    if operand.starts_with('%') {
        return Some(Target::Job(operand));
    }
    if let Some(group) = operand.strip_prefix('-') {
        return group
            .parse::<u32>()
            .ok()
            .filter(|group| *group > 0)
            .map(Target::Group);
    }
    operand.parse::<u32>().ok().map(Target::Pid)
}

/// `kill [-s SIG | -SIG | -n NUM] PID|%JOB|-PGID...` and `kill -l [SIG|NUM]...`. Every operand is
/// checked before any signal is sent.
pub(in crate::embedded_shell::builtins) async fn kill(
    io: &mut Io<'_>,
    context: &mut ExecutionContext<'_>,
    args: &[String],
) -> i32 {
    let mut signal = DEFAULT_SIGNAL;
    let mut listing = false;
    let mut signalled = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--" => {
                index += 1;
                break;
            }
            "-l" | "-L" if !signalled => {
                listing = true;
                index += 1;
                break;
            }
            "-s" | "-n" if !signalled => {
                let Some(value) = args.get(index + 1) else {
                    return usage_error(
                        io,
                        &format!("option requires an argument -- '{}'", &arg[1..]),
                    );
                };
                let Some(number) = signal_number(value) else {
                    return io.fail(format!("{value}: invalid signal specification"));
                };
                signal = number;
                signalled = true;
                index += 2;
            }
            _ if !signalled && arg.len() > 1 && arg.starts_with('-') => {
                let Some(number) = signal_number(&arg[1..]) else {
                    return io.fail(format!("{}: invalid signal specification", &arg[1..]));
                };
                signal = number;
                signalled = true;
                index += 1;
            }
            _ => break,
        }
    }
    let operands = &args[index..];
    if listing {
        return list(io, operands);
    }
    if operands.is_empty() {
        return usage_error(io, "no process or job specified");
    }
    let mut targets = Vec::with_capacity(operands.len());
    for operand in operands {
        let Some(target) = parse_target(operand) else {
            let _ = writeln!(
                io.stderr,
                "kill: {operand}: arguments must be process or job IDs"
            );
            return 2;
        };
        targets.push(target);
    }
    let mut status = 0;
    for target in targets {
        match target {
            Target::Job(spec) => match signal_job_target(context, spec, signal).await {
                Some(Ok(())) => {}
                Some(Err(error)) => status = io.fail(format!("{spec}: {}", err_text(&error))),
                None => status = io.fail(format!("{spec}: no such job")),
            },
            Target::Pid(pid) => {
                let label = pid.to_string();
                match signal_job_target(context, &label, signal).await {
                    Some(Ok(())) => {}
                    Some(Err(error)) => status = report(io, &label, &error),
                    None => {
                        if let Err(error) = send(pid, signal) {
                            status = report(io, &label, &error);
                        }
                    }
                }
            }
            Target::Group(group) => {
                if let Err(error) = send_group(group, signal) {
                    status = report(io, &format!("-{group}"), &error);
                }
            }
        }
    }
    status
}

#[cfg(all(test, windows))]
mod tests;
