//! Where dial keeps things. Private state is normally outside the project. A containing workspace
//! (such as HOME) is allowed, with the state tree excluded from agent tools and sandboxed commands.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Layout {
    pub project_root: PathBuf,
    pub project_dir: PathBuf,
    pub user_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Layout {
    pub fn discover(cwd: &Path) -> Result<Self> {
        let user_dir = match std::env::var_os("DIAL_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set; set DIAL_HOME")?)
                .join(".dial"),
        };
        Self::with_user_dir(cwd, user_dir)
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
mod tests {
    use super::*;

    #[test]
    fn state_is_per_project_and_private_workspaces_are_rejected() {
        let home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (la, lb) = (
            Layout::with_user_dir(a.path(), home.path().into()).unwrap(),
            Layout::with_user_dir(b.path(), home.path().into()).unwrap(),
        );
        assert_ne!(la.state_dir, lb.state_dir);
        la.ensure_project_outside_storage().unwrap();
        let inside = Layout::with_user_dir(a.path(), a.path().join(".dialhome")).unwrap();
        inside.ensure_project_outside_storage().unwrap();
        let private_project = a.path().join(".dialhome/private");
        fs::create_dir_all(&private_project).unwrap();
        let private = Layout::with_user_dir(&private_project, a.path().join(".dialhome")).unwrap();
        assert!(private.ensure_project_outside_storage().is_err());
    }

    #[test]
    fn home_directory_can_be_the_project_without_moving_state() {
        let home = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(home.path(), home.path().join(".dial")).unwrap();
        layout.ensure_project_outside_storage().unwrap();
        assert_eq!(layout.project_root, fs::canonicalize(home.path()).unwrap());
        assert_eq!(layout.user_dir, layout.project_root.join(".dial"));
        assert!(layout.state_dir.starts_with(&layout.user_dir));
        assert_eq!(
            layout.config_files()[0],
            layout.user_dir.join("config.toml")
        );
    }
}
