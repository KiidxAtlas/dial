//! Built-in POSIX shell for the `bash` tool (`shell.backend = "embedded"`).
//!
//! The shell runs in a child process: the Dial binary re-executed with [`HOST_ARG`] and the path
//! of a one-shot script file. That keeps pipes, redirects, process groups, timeouts, and the OS
//! sandbox identical to running `/bin/bash`. The shell language and POSIX builtins (`cd`, `echo`,
//! `printf`, `test`, `read`, `export`, ...) come from brush; the common coreutils are in
//! [`builtins`] and are resolved before `PATH`.

#[cfg(feature = "embedded-shell")]
pub mod builtins;
pub mod state;

#[cfg(any(test, feature = "embedded-shell"))]
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// argv[1] that makes the Dial binary act as the embedded shell host.
pub const HOST_ARG: &str = "__embedded-shell";
/// Test hook: overrides the executable used as the embedded shell host.
pub const HOST_EXE_ENV: &str = "DIAL_EMBEDDED_SHELL_HOST";
/// File-name prefix of script files handed to the host; the host only removes files with it.
const SCRIPT_PREFIX: &str = "dial-shell-";

/// Executable that hosts the embedded shell: `DIAL_EMBEDDED_SHELL_HOST` if set, else this binary.
pub fn host_exe() -> io::Result<PathBuf> {
    match std::env::var_os(HOST_EXE_ENV) {
        Some(path) => Ok(PathBuf::from(path)),
        None => std::env::current_exe(),
    }
}

/// Writes `script` to a fresh single-use file in `scratch` and returns the host program and its
/// arguments. The host deletes the file after reading it.
pub fn invocation(scratch: &Path, script: &str) -> io::Result<(PathBuf, Vec<String>)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = scratch.join(format!(
        "{SCRIPT_PREFIX}{}-{nanos}-{n}.sh",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(script.as_bytes())?;
    file.flush()?;
    Ok((
        host_exe()?,
        vec![HOST_ARG.to_owned(), path.display().to_string()],
    ))
}

/// Runs a script file as the embedded shell host and returns the process exit code.
#[cfg(feature = "embedded-shell")]
pub fn run_host(script_path: &Path) -> i32 {
    run_host_with_state(script_path, None)
}

#[cfg(feature = "embedded-shell")]
pub fn run_host_with_state(script_path: &Path, state_path: Option<&Path>) -> i32 {
    let seed = match state_path.map(state::read).transpose() {
        Ok(seed) => seed,
        Err(error) => {
            eprintln!("dial: {error}");
            return 2;
        }
    };
    use brush_builtins::{BuiltinSet, ShellBuilderExt};
    use brush_core::{ProfileLoadBehavior, RcLoadBehavior, SourceInfo};

    let script = fs::read_to_string(script_path);
    // The script file is single-use and only ever ours; remove it whatever happened.
    if script_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(SCRIPT_PREFIX))
    {
        let _ = fs::remove_file(script_path);
    }
    let script = match script {
        Ok(script) => script,
        Err(err) => {
            eprintln!(
                "dial: cannot read shell script {}: {err}",
                script_path.display()
            );
            return 2;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("dial: cannot start the embedded shell runtime: {err}");
            return 2;
        }
    };
    runtime.block_on(async move {
        let mut shell = match brush_core::Shell::builder()
            .default_builtins(BuiltinSet::BashMode)
            .builtins(builtins::registrations())
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .interactive(false)
            .shell_name("bash".to_owned())
            .build()
            .await
        {
            Ok(shell) => shell,
            Err(err) => {
                eprintln!("dial: cannot start the embedded shell: {err}");
                return 2;
            }
        };
        if let Some(seed) = &seed {
            for (name, value) in &seed.exported {
                let mut variable = brush_core::variables::ShellVariable::new(value.clone());
                variable.export();
                if let Err(error) = shell.env_mut().set_global(name, variable) {
                    eprintln!("dial: restoring shell state: {error}");
                    return 2;
                }
            }
        }
        let params = shell.default_exec_params();
        let source = SourceInfo::default();
        match shell.run_string(script, &source, &params).await {
            Ok(result) => {
                if let Some(path) = state_path {
                    let exported = shell
                        .env()
                        .iter_exported()
                        .filter_map(|(name, variable)| {
                            if let brush_core::variables::ShellValue::String(value) =
                                variable.value()
                                && state::allowed(name, value)
                            {
                                Some((name.clone(), value.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    let state = state::State {
                        schema_version: 1,
                        completed: true,
                        cwd: shell.working_dir().to_owned(),
                        exported,
                    };
                    if let Err(error) = state.validate().and_then(|()| {
                        crate::storage::settings_store::write_private(
                            path,
                            &serde_json::to_vec(&state).map_err(|error| error.to_string())?,
                        )
                        .map_err(|error| error.to_string())
                    }) {
                        eprintln!("dial: saving shell state: {error}");
                        return 125;
                    }
                }
                i32::from(u8::from(result.exit_code))
            }
            Err(err) => {
                eprintln!("dial: {err}");
                2
            }
        }
    })
}

#[cfg(test)]
mod tests;
