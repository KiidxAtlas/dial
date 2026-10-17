//! External audio programs: PATH lookup and cancellable runs with bounded output.

use anyhow::{Result, bail};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::atomic::AtomicBool,
    time::Duration,
};

/// One program invocation, built separately from running it so construction is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Arguments as UTF-8 strings (lossy), for tests and error messages.
    #[cfg(test)]
    pub fn arg_strings(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

/// Resolve a program name on `PATH`; a name with a separator must name an executable file.
pub fn which(program: &Path) -> Option<PathBuf> {
    if program.as_os_str().is_empty() {
        return None;
    }
    if program.components().count() > 1 {
        return executable_candidate(program);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter_map(|dir| executable_candidate(&dir.join(program)))
        .next()
}

fn executable_candidate(path: &Path) -> Option<PathBuf> {
    if executable(path) {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.COM;.BAT;.CMD".into());
        for extension in extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let candidate = path.with_extension(extension.trim_start_matches('.'));
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
        && crate::platform::fs::is_executable(path)
}
/// Captured result of [`run`].
pub struct Finished {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

const MAX_STDOUT: usize = 64 * 1024 * 1024;
const MAX_STDERR: usize = 16 * 1024;

/// Run `spec` to completion, feeding `stdin` and collecting bounded output. The process is killed
/// when `cancel` is set or `timeout` passes. A nonzero exit is an error carrying stderr.
pub fn run(
    spec: &CommandSpec,
    stdin: Option<&[u8]>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<Finished> {
    let mut policy = crate::runtime::process::Policy::bounded(timeout, MAX_STDOUT);
    policy.stderr_cap = MAX_STDERR;
    let output = crate::runtime::process::run_with_policy(
        spec.command(),
        stdin.map(<[u8]>::to_vec),
        policy,
        cancel,
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)?;
    if !output.success() {
        bail!(
            "{}",
            output.failure(&spec.program.display().to_string(), timeout)
        );
    }
    if output.stdout_total > MAX_STDOUT {
        bail!(
            "{} wrote more than {} MiB to stdout",
            spec.program.display(),
            MAX_STDOUT >> 20
        );
    }
    let stderr = output.stderr_text(MAX_STDERR);
    Ok(Finished {
        stdout: output.stdout,
        stderr,
    })
}

/// Kill `child` and its whole process group, so a shell pipeline cannot keep the captured pipes open
/// after its leader dies. `child` must have been spawned with `platform::process::spawn_group`.
pub fn kill(child: &mut Child) {
    let _ = crate::platform::process::kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests;
