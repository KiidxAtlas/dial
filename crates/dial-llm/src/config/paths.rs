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
        [
            self.project_dir.join("skills"),
            self.user_dir.join("skills"),
        ]
    }
    pub fn config_files(&self) -> [PathBuf; 2] {
        [
            self.user_dir.join("config.toml"),
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

#[cfg(test)]
mod tests;
