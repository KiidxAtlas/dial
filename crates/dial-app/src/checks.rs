//! Project verification checks, shared by `/checks` and the RPC `list_checks`, `add_check` and
//! `remove_check` methods: the automatic plan for uncommitted changes (the same planner that runs
//! after each edit) and the project-level `verification.commands` list that `/checks add` writes.
use crate::agent::{plan_with_runner, related_tests_with_runner};
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Change};
use crate::config::types::{Config, VerificationCommand};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Settings path of the project check list.
const COMMANDS_PATH: &str = "verification.commands";

/// The automatic checks for the working tree and the files they were planned from.
pub(crate) struct Plan {
    pub(crate) changed: Vec<String>,
    pub(crate) automatic: Vec<AutomaticCheck>,
}

/// One automatic check: what runs and how long it may take.
pub(crate) struct AutomaticCheck {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) timeout_secs: u64,
}

/// Automatic checks for the uncommitted changes (the planner that runs after each edit), plus the
/// related tests when selective verification is on.
pub(crate) fn plan(config: &Config) -> Plan {
    let root = &config.project_root;
    let changed = changed_files(root);
    let paths: Vec<PathBuf> = changed.iter().map(PathBuf::from).collect();
    let python = if config.python.is_file() {
        config.python.clone()
    } else {
        PathBuf::from("python3")
    };
    let mut automatic =
        plan_with_runner(root, &python, &paths, &config.verification.python_command);
    if config.assistance.enabled && config.assistance.selective_verification {
        automatic.extend(related_tests_with_runner(
            root,
            &python,
            &paths,
            &config.verification.python_command,
        ));
    }
    let automatic = automatic
        .into_iter()
        .map(|check| AutomaticCheck {
            name: check.name,
            command: check.command,
            timeout_secs: check.timeout_secs,
        })
        .collect();
    Plan { changed, automatic }
}

/// Paths with uncommitted changes in the project, relative to the project root, as `git status`
/// reports them. Empty outside Git. Git reports paths relative to the repository top, so a project
/// nested below it strips its own prefix and drops changes outside it.
fn changed_files(root: &Path) -> Vec<String> {
    let Ok(prefix) =
        crate::tools::vcs::git::run_checked(root, &["rev-parse", "--show-prefix"], false)
    else {
        return Vec::new();
    };
    // `-z` keeps paths verbatim (no C-style quoting, no `->` rename text), and `--no-renames`
    // gives each side of a rename its own `XY path` record.
    let Ok(stdout) = crate::tools::vcs::git::run_checked(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        false,
    ) else {
        return Vec::new();
    };
    parse_porcelain_z(&stdout, prefix.trim_end_matches(['\n', '\r']))
}

/// Paths from NUL-terminated `git status --porcelain=v1 -z --no-renames` records (`XY path`),
/// made relative to `prefix` (the project's directory below the repository top, `""` at the top).
/// Records outside that directory are not project changes.
fn parse_porcelain_z(output: &str, prefix: &str) -> Vec<String> {
    output
        .split('\0')
        .filter_map(|record| record.get(3..))
        .filter_map(|path| {
            if prefix.is_empty() {
                Some(path)
            } else {
                path.strip_prefix(prefix)
            }
        })
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The project-level check list as saved, read raw so user-level entries are not copied down.
#[cfg(test)]
pub(crate) fn project_commands(layout: &Layout) -> Result<Vec<toml::Value>> {
    let project =
        settings_store::load_layer(layout, crate::config::settings_store::Scope::Project)?;
    Ok(project
        .get("verification")
        .and_then(|value| value.get("commands"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Removes the project entry named `name`; false when the project file has none.
pub(crate) fn remove_named(commands: &mut Vec<toml::Value>, name: &str) -> bool {
    let before = commands.len();
    commands.retain(|value| value.get("name").and_then(toml::Value::as_str) != Some(name));
    commands.len() != before
}

/// The settings change that records the project list `config` now holds, for the adoption step
/// after a shared-owner write. It names the same path the owner wrote.
pub(crate) fn saved_change(config: &Config) -> Result<Change> {
    Ok(Change::Set {
        path: COMMANDS_PATH.into(),
        value: toml::Value::try_from(&config.verification.commands)?,
    })
}

/// Why a project check edit was refused. Nothing was published; callers map the variant to their
/// own error code.
#[derive(Debug)]
pub(crate) enum EditRefused {
    /// A check with this name is already configured in the user or the project file.
    Exists(String),
    /// The project file has no check with this name.
    Missing(String),
}

impl std::fmt::Display for EditRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(name) => write!(f, "check {name} already exists; remove it first."),
            Self::Missing(name) => write!(
                f,
                "no check named {name} in the project config; checks from the user config must be removed from that file"
            ),
        }
    }
}

impl std::error::Error for EditRefused {}

/// Appends `check` to the project list unless the effective configuration (either layer) already
/// has its name. The check and the write happen under both settings locks, against the saved
/// files rather than any session snapshot, so a removed name can be added again at once.
pub(crate) fn add_project(layout: &Layout, check: &VerificationCommand) -> Result<Config> {
    let value = toml::Value::try_from(check)?;
    settings_store::update_project_array(layout, COMMANDS_PATH, |commands, current| {
        if current
            .verification
            .commands
            .iter()
            .any(|existing| existing.name == check.name)
        {
            return Err(EditRefused::Exists(check.name.clone()).into());
        }
        commands.push(value);
        Ok(())
    })
}

/// Removes the project check `name` under the same locks as `add_project`.
pub(crate) fn remove_project(layout: &Layout, name: &str) -> Result<Config> {
    settings_store::update_project_array(layout, COMMANDS_PATH, |commands, _| {
        if remove_named(commands, name) {
            Ok(())
        } else {
            Err(EditRefused::Missing(name.into()).into())
        }
    })
}
