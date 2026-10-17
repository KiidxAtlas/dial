//! Project Python environments for the kernel: an active or project virtualenv, or a conda prefix.
//!
//! Nothing inside the workspace runs outside the sandbox. A virtualenv contributes only its
//! `site-packages`, added by the sandboxed driver; the interpreter that runs is the
//! operator-installed base named by `pyvenv.cfg`, resolved and preflighted like `kernel.python`.
//! A conda prefix is itself an interpreter installation and must live outside the workspace.
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnvKind {
    Venv,
    Conda,
}

impl EnvKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            EnvKind::Venv => "venv",
            EnvKind::Conda => "conda",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PythonEnv {
    pub kind: EnvKind,
    pub prefix: PathBuf,
    /// Base interpreter candidates, best first. Each must still resolve outside the workspace.
    pub launchers: Vec<PathBuf>,
    /// `include-system-site-packages` from `pyvenv.cfg`; conda prefixes own their site.
    pub include_system_site: bool,
    /// Where the environment came from: an environment variable, a project directory or config.
    pub source: String,
}

/// Select the project environment for `kernel.venv`:
/// * `"off"`: none.
/// * `"auto"` (default): only while `kernel.python` is the default launcher. `VIRTUAL_ENV`, then
///   `CONDA_PREFIX`, then `<workspace>/.venv`, then `<workspace>/venv`; unusable candidates are skipped.
/// * anything else: an explicit environment prefix (relative to the workspace), which must be valid.
pub(crate) fn detect(
    workspace: &Path,
    setting: &str,
    python: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<PythonEnv>, String> {
    match setting.trim() {
        "off" | "none" | "" => Ok(None),
        "auto" => {
            if python != super::DEFAULT_PYTHON {
                return Ok(None);
            }
            let mut candidates = Vec::new();
            for variable in ["VIRTUAL_ENV", "CONDA_PREFIX"] {
                if let Some(value) = lookup(variable).filter(|value| !value.trim().is_empty()) {
                    candidates.push((PathBuf::from(value.trim()), variable.to_owned()));
                }
            }
            for name in [".venv", "venv"] {
                candidates.push((workspace.join(name), format!("workspace {name}")));
            }
            Ok(candidates
                .into_iter()
                .filter(|(prefix, _)| prefix.is_absolute())
                .find_map(|(prefix, source)| inspect(&prefix, source)))
        }
        explicit => {
            let path = Path::new(explicit);
            let prefix = if path.is_absolute() {
                path.to_path_buf()
            } else {
                workspace.join(path)
            };
            inspect(&prefix, "kernel.venv".into()).map(Some).ok_or_else(|| {
                format!(
                    "kernel.venv {explicit:?} is neither a virtualenv (pyvenv.cfg) nor a conda prefix (conda-meta)"
                )
            })
        }
    }
}

fn inspect(prefix: &Path, source: String) -> Option<PythonEnv> {
    inspect_layout(prefix, source, cfg!(windows))
}

// An explicit layout keeps both platforms covered by filesystem fixtures on every test host.
fn inspect_layout(prefix: &Path, source: String, windows: bool) -> Option<PythonEnv> {
    let config = prefix.join("pyvenv.cfg");
    if config.is_file() {
        let text = std::fs::read_to_string(&config).ok()?;
        let settings = parse_pyvenv_cfg(&text);
        let value = |key: &str| {
            settings
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        let mut launchers = Vec::new();
        // Windows venv executables are redirectors, not the base interpreter. Use the absolute
        // configured base; every candidate still passes resolve_operator_executable/preflight.
        if windows {
            if let Some(executable) = value("executable")
                .or(value("base-executable"))
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
            {
                launchers.push(executable);
            }
        } else {
            // POSIX venv links normally resolve to the base; workspace copies are rejected later.
            launchers.push(prefix.join("bin").join("python"));
        }
        if let Some(home) = value("home")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
        {
            if windows {
                launchers.push(home.join("python.exe"));
            } else {
                if let Some(minor) = value("version_info")
                    .or(value("version"))
                    .and_then(major_minor)
                {
                    launchers.push(home.join(format!("python{}.{}", minor.0, minor.1)));
                }
                launchers.push(home.join("python3"));
                launchers.push(home.join("python"));
            }
        }
        return Some(PythonEnv {
            kind: EnvKind::Venv,
            prefix: prefix.to_path_buf(),
            launchers,
            include_system_site: value("include-system-site-packages")
                .is_some_and(|flag| flag.eq_ignore_ascii_case("true")),
            source,
        });
    }
    if prefix.join("conda-meta").is_dir() {
        let bin = prefix.join("bin");
        return Some(PythonEnv {
            kind: EnvKind::Conda,
            prefix: prefix.to_path_buf(),
            launchers: if windows {
                vec![prefix.join("python.exe")]
            } else {
                vec![bin.join("python3"), bin.join("python")]
            },
            include_system_site: true,
            source,
        });
    }
    None
}

/// `key = value` lines; keys lowercased, `#` comments and blank lines ignored.
pub(crate) fn parse_pyvenv_cfg(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    Some((major, minor))
}

/// The venv's `site-packages` for the interpreter version that will actually run it.
pub(crate) fn site_packages(prefix: &Path, version: (u64, u64)) -> Result<PathBuf, String> {
    site_packages_layout(prefix, version, cfg!(windows))
}

fn site_packages_layout(
    prefix: &Path,
    version: (u64, u64),
    windows: bool,
) -> Result<PathBuf, String> {
    if windows {
        let wanted = prefix.join("Lib").join("site-packages");
        return wanted.is_dir().then_some(wanted).ok_or_else(|| {
            format!(
                "virtualenv {} has no Lib/site-packages; recreate it with the base interpreter or set kernel.venv = \"off\"",
                prefix.display()
            )
        });
    }
    let wanted = prefix
        .join("lib")
        .join(format!("python{}.{}", version.0, version.1))
        .join("site-packages");
    if wanted.is_dir() {
        return Ok(wanted);
    }
    let found: Vec<String> = std::fs::read_dir(prefix.join("lib"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().join("site-packages").is_dir())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    Err(format!(
        "virtualenv {} has no site-packages for Python {}.{} (found: {}); recreate it with the base interpreter or set kernel.venv = \"off\"",
        prefix.display(),
        version.0,
        version.1,
        if found.is_empty() {
            "none".to_owned()
        } else {
            found.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests;
