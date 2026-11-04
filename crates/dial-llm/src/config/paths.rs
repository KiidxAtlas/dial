//! Where dial keeps things. Private state is normally outside the project. A containing workspace
//! (such as HOME) is allowed, with the state tree excluded from agent tools and sandboxed commands.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// The user's home directory. Keep HOME authoritative; native Windows also supplies USERPROFILE.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(std::env::var_os("HOME"), cfg!(windows), || {
        std::env::var_os("USERPROFILE")
    })
}

fn home_dir_from(
    home: Option<std::ffi::OsString>,
    windows: bool,
    profile: impl FnOnce() -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty())
        .or_else(|| {
            windows
                .then(profile)
                .flatten()
                .filter(|value| !value.is_empty())
        })
        .map(PathBuf::from)
}

/// Dial's private user directory, with an explicit DIAL_HOME taking precedence over the home.
pub fn user_dir() -> Result<PathBuf> {
    user_dir_from(std::env::var_os("DIAL_HOME"), home_dir)
}

fn user_dir_from(
    dial_home: Option<std::ffi::OsString>,
    home: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    dial_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".dial")))
        .context("home directory is unavailable; set DIAL_HOME (or HOME; USERPROFILE on Windows)")
}

/// Subscription authentication may explicitly share a store while all other state stays isolated.
pub const SUBSCRIPTION_AUTH_HOME: &str = "DIAL_SUBSCRIPTION_AUTH_HOME";

pub fn subscription_auth_dir(user_dir: &Path) -> Result<PathBuf> {
    subscription_auth_dir_from(user_dir, std::env::var_os(SUBSCRIPTION_AUTH_HOME))
}

fn subscription_auth_dir_from(
    user_dir: &Path,
    shared: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    let Some(shared) = shared else {
        return Ok(user_dir.to_owned());
    };
    let path = PathBuf::from(shared);
    ensure!(
        path.is_absolute(),
        "DIAL_SUBSCRIPTION_AUTH_HOME must be an absolute directory"
    );
    let path =
        fs::canonicalize(path).context("opening shared subscription authentication directory")?;
    ensure!(
        path.is_dir(),
        "shared subscription authentication path must be a directory"
    );
    Ok(path)
}

/// Small private records are copied lazily; the legacy record is never removed. A busy or
/// inaccessible legacy store remains authoritative until a later resolution can copy it safely.
pub fn grouped_file(user_dir: &Path, group: &str, name: &str) -> PathBuf {
    let legacy = user_dir.join(name);
    let target = user_dir.join(group).join(name);
    register_lock_anchor(&target, &legacy);
    if target.exists() {
        return target;
    }
    if !legacy.exists() {
        return if crate::platform::atomic::private_dir(target.parent().unwrap()).is_ok() {
            target
        } else {
            legacy
        };
    }
    let copied = (|| -> Result<()> {
        let _legacy = super::settings_store::try_locked(&legacy)?;
        // Competing resolvers may have published while this process waited for the lock.
        if target.exists() {
            return Ok(());
        }
        let bytes = crate::platform::atomic::read_private(&legacy)?
            .context("legacy private record disappeared")?;
        crate::platform::atomic::private_dir(target.parent().unwrap())?;
        crate::platform::atomic::write_private(&target, &bytes)
    })();
    if copied.is_ok() { target } else { legacy }
}

pub fn auth_file(user_dir: &Path, name: &str) -> PathBuf {
    grouped_file(user_dir, "auth", name)
}
pub fn state_file(user_dir: &Path, name: &str) -> PathBuf {
    grouped_file(user_dir, "state", name)
}
pub fn cache_file(user_dir: &Path, name: &str) -> PathBuf {
    grouped_file(user_dir, "cache", name)
}

/// Grouped records retain their original lock inode, so older Dial processes and migration
/// resolvers still serialize with new writers. Lock files are deliberately never copied.
static LOCK_ANCHORS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));

fn register_lock_anchor(target: &Path, legacy: &Path) {
    LOCK_ANCHORS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(target.to_owned(), legacy.to_owned());
}

pub(crate) fn record_lock_anchor(path: &Path) -> PathBuf {
    LOCK_ANCHORS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_owned())
}

pub fn model_catalog_file(user_dir: &Path) -> PathBuf {
    cache_file(user_dir, "model-catalog.json")
}
pub fn learned_words_file(user_dir: &Path) -> PathBuf {
    state_file(user_dir, "learned-words.json")
}
pub fn tour_seen_file(user_dir: &Path) -> PathBuf {
    state_file(user_dir, "tour-seen")
}
pub fn chatgpt_model_catalog_file(user_dir: &Path) -> PathBuf {
    cache_file(user_dir, "chatgpt-model-catalog.json")
}

// Large trees, editable resources, databases (including WAL sidecars) and live coordination
// inodes deliberately stay in place. Only bounded private records use grouped_file above.
macro_rules! stable_user_paths {
    ($($name:ident => $relative:literal),* $(,)?) => {
        $(pub fn $name(user_dir: &Path) -> PathBuf { user_dir.join($relative) })*
    };
}
stable_user_paths! {
    agents_dir => "agents",
    agents_file => "AGENTS.md",
    soul_file => "SOUL.md",
    commands_dir => "commands",
    themes_dir => "themes",
    skills_dir => "skills",
    skills_file => "skills.json",
    skill_packages_dir => "skill-packages",
    projects_dir => "projects",
    python_dir => "python",
    tools_dir => "tools",
    laya_dir => "laya",
    mcp_dir => "mcp",
    run_dir => "run",
    ssh_dir => "ssh",
    security_dir => "security",
    browser_relay_dir => "browser-relay",
    browser_extension_dir => "browser-extension",
    provider_batches_dir => "provider-batches",
    background_jobs_dir => "background-jobs",
    compute_lock_file => "compute.lock",
    foreground_last_file => "foreground-last",
    compute_usage_file => "compute-usage.json",
    stats_db_file => "stats.sqlite3",
    session_pins_file => "session-pins.json",
    desktop_dir => "desktop",
    worktrees_dir => "wt",
    piper_models_dir => "models/piper",
    config_file => "config.toml",
    history_db_file => "history.db",
    gc_db_file => "gc.sqlite3",
    images_dir => "images",
    secret_mask_key_file => "secret-mask.key",
    smithery_key_file => "smithery.json",
    local_profile_file => "local-profile.json",
    usage_history_file => "usage-history.jsonl",
    session_transfers_file => "session-transfers.json",
    session_transfers_lock_file => "session-transfers.lock",
    release_check_file => "dial-release-check",
    runtime_update_check_file => "runtime-update-check",
    commit_cache_file => "cache/commit-inference.db",
    judgment_cache_file => "cache/judgment.db",
    models_dir => "models",
    rules_dir => "rules",
    memory_backends_dir => "memory-backends",
}

pub fn oauth_refresh_lock_file(user_dir: &Path, id: &str, digest: &str) -> PathBuf {
    user_dir.join(format!("oauth-{id}.{digest}.refresh"))
}
pub fn mcp_oauth_file(user_dir: &Path, encoded_server: &str) -> PathBuf {
    user_dir.join(format!("mcp-oauth-{encoded_server}.json"))
}
pub fn session_pins_temp_file(user_dir: &Path, nonce: &str) -> PathBuf {
    user_dir.join(format!(".session-pins.{}.{nonce}.tmp", std::process::id()))
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub project_root: PathBuf,
    pub project_dir: PathBuf,
    pub user_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Layout {
    pub fn discover(cwd: &Path) -> Result<Self> {
        Self::with_user_dir(cwd, user_dir()?)
    }

    pub fn with_user_dir(cwd: &Path, user_dir: PathBuf) -> Result<Self> {
        let project_root = fs::canonicalize(cwd)
            .with_context(|| format!("project directory {} does not exist", cwd.display()))?;
        ensure!(project_root.is_dir(), "project path must be a directory");
        ensure!(
            project_root.parent().is_some(),
            "refusing to use the filesystem root as a project"
        );
        let user_dir = resolve_prefix(&if user_dir.is_absolute() {
            user_dir
        } else {
            std::env::current_dir()?.join(user_dir)
        });
        let name: String = project_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(40)
            .collect();
        let hash = format!(
            "{:x}",
            Sha256::digest(project_root.as_os_str().as_encoded_bytes())
        );
        let state_dir = user_dir
            .join("projects")
            .join(format!("{name}-{}", &hash[..8]));
        Ok(Self {
            project_dir: project_root.join(".dial"),
            project_root,
            user_dir,
            state_dir,
        })
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }
    pub fn checkpoints_dir(&self) -> PathBuf {
        self.state_dir.join("checkpoints")
    }
    pub fn project_memory_file(&self) -> PathBuf {
        self.state_dir.join("MEMORY.md")
    }
    pub fn user_memory_file(&self) -> PathBuf {
        self.user_dir.join("USER.md")
    }
    pub fn skills_dirs(&self) -> [PathBuf; 2] {
        [self.project_dir.join("skills"), skills_dir(&self.user_dir)]
    }
    pub fn config_files(&self) -> [PathBuf; 2] {
        [
            config_file(&self.user_dir),
            self.state_dir.join("config.toml"),
        ]
    }

    /// A workspace may contain private storage, but must not itself be inside it: tools exclude
    /// the private tree, which would otherwise exclude the entire workspace.
    pub fn ensure_project_outside_storage(&self) -> Result<()> {
        ensure!(
            !self.project_root.starts_with(&self.user_dir)
                && !self.project_root.starts_with(&self.state_dir),
            "project {} is inside dial's private storage {}; choose a different project directory",
            self.project_root.display(),
            self.user_dir.display()
        );
        Ok(())
    }
}

/// Canonicalize the longest existing prefix (the directory may not exist yet) so symlinked
/// locations such as macOS `/var` -> `/private/var` compare equal.
fn resolve_prefix(path: &Path) -> PathBuf {
    let mut probe = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(real) = fs::canonicalize(probe) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (probe.parent(), probe.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                probe = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Legacy shortcut file, retained as a per-binding fallback for config.toml [keys].
pub fn keybindings_file(user_dir: &Path) -> PathBuf {
    user_dir.join("keybindings.toml")
}

#[cfg(test)]
mod tests;
