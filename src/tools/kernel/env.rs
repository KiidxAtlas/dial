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
mod tests {
    use super::*;

    fn venv(root: &Path, name: &str, config: &str) -> PathBuf {
        let prefix = root.join(name);
        std::fs::create_dir_all(prefix.join("lib/python3.12/site-packages")).unwrap();
        std::fs::write(prefix.join("pyvenv.cfg"), config).unwrap();
        prefix
    }

    #[test]
    fn pyvenv_cfg_parses_home_version_and_system_site() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = venv(
            dir.path(),
            ".venv",
            &format!(
                "home = {}\n# note\ninclude-system-site-packages = false\nversion = 3.12.4\n",
                dir.path().join("base/bin").display()
            ),
        );
        let found = inspect_layout(&prefix, "workspace .venv".into(), false).unwrap();
        assert_eq!(found.kind, EnvKind::Venv);
        assert_eq!(found.prefix, prefix);
        assert!(!found.include_system_site);
        assert_eq!(
            found.launchers,
            vec![
                prefix.join("bin/python"),
                dir.path().join("base/bin/python3.12"),
                dir.path().join("base/bin/python3"),
                dir.path().join("base/bin/python"),
            ]
        );
        assert_eq!(found.source, "workspace .venv");
    }

    #[test]
    fn auto_detection_order_and_opt_outs() {
        let dir = tempfile::tempdir().unwrap();
        let active = venv(dir.path(), "active", "home = /usr/bin\n");
        venv(dir.path(), ".venv", "home = /usr/bin\n");
        let conda = dir.path().join("conda");
        std::fs::create_dir_all(conda.join("conda-meta")).unwrap();
        let both = |name: &str| match name {
            "VIRTUAL_ENV" => Some(active.to_string_lossy().into_owned()),
            "CONDA_PREFIX" => Some(conda.to_string_lossy().into_owned()),
            _ => None,
        };
        let found = detect(dir.path(), "auto", "python3", &both)
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.prefix.clone(), found.source.as_str()),
            (active.clone(), "VIRTUAL_ENV")
        );
        let conda_only =
            |name: &str| (name == "CONDA_PREFIX").then(|| conda.to_string_lossy().into_owned());
        let found = detect(dir.path(), "auto", "python3", &conda_only)
            .unwrap()
            .unwrap();
        assert_eq!(found.kind, EnvKind::Conda);
        // A stale VIRTUAL_ENV is skipped rather than failing the kernel.
        let stale = |name: &str| (name == "VIRTUAL_ENV").then(|| "/nonexistent/venv".to_owned());
        let found = detect(dir.path(), "auto", "python3", &stale)
            .unwrap()
            .unwrap();
        assert_eq!(found.source, "workspace .venv");
        assert!(
            detect(dir.path(), "off", "python3", &both)
                .unwrap()
                .is_none()
        );
        // An explicitly configured interpreter bypasses discovery.
        assert!(
            detect(dir.path(), "auto", "/opt/python3.11", &both)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_setting_must_name_an_environment() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = venv(dir.path(), "envs/data", "home = /usr/bin\n");
        let found = detect(dir.path(), "envs/data", "/opt/python", &|_| None)
            .unwrap()
            .unwrap();
        assert_eq!(found.prefix, prefix);
        assert_eq!(found.source, "kernel.venv");
        assert!(
            detect(dir.path(), "missing", "python3", &|_| None)
                .unwrap_err()
                .contains("neither")
        );
    }

    #[test]
    fn site_packages_must_match_the_running_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = venv(dir.path(), ".venv", "home = /usr/bin\n");
        assert_eq!(
            site_packages_layout(&prefix, (3, 12), false).unwrap(),
            prefix.join("lib/python3.12/site-packages")
        );
        let error = site_packages_layout(&prefix, (3, 11), false).unwrap_err();
        assert!(
            error.contains("python3.12") && error.contains("3.11"),
            "{error}"
        );
    }

    #[test]
    fn windows_venv_uses_base_interpreter_and_unversioned_site() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Python312");
        let executable = base.join("python.exe");
        let prefix = venv(
            dir.path(),
            ".venv",
            &format!(
                "home = {}\nexecutable = {}\nversion = 3.12.4\ninclude-system-site-packages = true\n",
                base.display(),
                executable.display()
            ),
        );
        std::fs::create_dir_all(prefix.join("Scripts")).unwrap();
        std::fs::write(prefix.join("Scripts/python.exe"), "redirector").unwrap();
        std::fs::create_dir_all(prefix.join("Lib/site-packages")).unwrap();
        let found = inspect_layout(&prefix, "fixture".into(), true).unwrap();
        assert_eq!(found.launchers, vec![executable.clone(), executable]);
        assert!(found.include_system_site);
        assert_eq!(
            site_packages_layout(&prefix, (3, 12), true).unwrap(),
            prefix.join("Lib/site-packages")
        );
        std::fs::remove_dir(prefix.join("Lib/site-packages")).unwrap();
        assert!(site_packages_layout(&prefix, (3, 12), true).is_err());
    }

    #[test]
    fn windows_venv_home_fallback_and_relative_base_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Python312");
        let prefix = venv(dir.path(), ".venv", &format!("home = {}\n", base.display()));
        let found = inspect_layout(&prefix, "fixture".into(), true).unwrap();
        assert_eq!(found.launchers, vec![base.join("python.exe")]);
        std::fs::write(
            prefix.join("pyvenv.cfg"),
            "home = relative\nexecutable = python.exe\n",
        )
        .unwrap();
        assert!(
            inspect_layout(&prefix, "fixture".into(), true)
                .unwrap()
                .launchers
                .is_empty()
        );
    }

    #[test]
    fn conda_interpreter_layouts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("conda-meta")).unwrap();
        for windows in [false, true] {
            let found = inspect_layout(dir.path(), "fixture".into(), windows).unwrap();
            assert_eq!(found.kind, EnvKind::Conda);
            assert_eq!(
                found.launchers,
                if windows {
                    vec![dir.path().join("python.exe")]
                } else {
                    vec![
                        dir.path().join("bin/python3"),
                        dir.path().join("bin/python"),
                    ]
                }
            );
        }
    }
}
