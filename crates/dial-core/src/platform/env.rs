//! Environment variables a child process needs when Dial starts it with a cleared environment.
//!
//! Windows programs fail in confusing ways without their system variables: Winsock cannot load
//! its providers without `SystemRoot`, and `ComSpec`/`PATHEXT` are needed to start `.cmd` shims.
//! Lists here name locations and system facts only; they never name credentials. Names are
//! matched case-insensitively on Windows, where the OS itself treats them that way.

use std::ffi::OsString;

/// Windows system variables a process with a cleared environment still needs.
pub const WINDOWS_SYSTEM_VARIABLES: &[&str] = &[
    "SystemRoot",
    "windir",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
];

/// Windows profile and temporary-directory variables. Programs that write temporary files or
/// per-user state look these up; a cleared environment otherwise sends them to fallbacks.
pub const WINDOWS_PROFILE_VARIABLES: &[&str] = &[
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
];

/// Names a cleared child should receive: `base` on every platform, plus the Windows system and
/// profile variables when `windows` is set. Never includes names outside these lists.
pub fn cleared_child_allow_list(base: &[&str], windows: bool) -> Vec<String> {
    let mut names: Vec<String> = base.iter().map(|name| (*name).to_owned()).collect();
    if windows {
        for name in WINDOWS_SYSTEM_VARIABLES
            .iter()
            .chain(WINDOWS_PROFILE_VARIABLES)
        {
            if !names.iter().any(|known| known.eq_ignore_ascii_case(name)) {
                names.push((*name).to_owned());
            }
        }
    }
    names
}

/// The entries of `vars` whose names appear in `allow`, in input order. `case_insensitive`
/// matches Windows name semantics. Values are never inspected or logged here.
pub fn allowed_from(
    allow: &[String],
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    case_insensitive: bool,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            allow.iter().any(|known| {
                if case_insensitive {
                    known.eq_ignore_ascii_case(&name)
                } else {
                    *known == name
                }
            })
        })
        .collect()
}

/// The Windows system variables among `vars`, matched in any letter case. It does not check the
/// platform; use [`windows_system_env`] for the live environment, which is empty off Windows.
pub fn windows_system_env_from(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let allow: Vec<String> = WINDOWS_SYSTEM_VARIABLES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    allowed_from(&allow, vars, true)
}

/// Windows system variables from Dial's own environment. Empty on other platforms.
pub fn windows_system_env() -> Vec<(OsString, OsString)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    windows_system_env_from(std::env::vars_os())
}

#[cfg(test)]
mod tests;
