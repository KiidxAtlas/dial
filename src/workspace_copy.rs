//! Shared source-copy and exclusion policy for isolated workspaces.
//!
//! Benchmark workspaces, the `copy` and `clone` isolation backends, overlay masks, the worktree
//! and projected backends, and private dependency copies all decide which project entries are
//! secrets, generated outputs, or dependency trees through this module.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// Directories that hold a project's private state or credentials. Overlay hides them from the
/// worker, and the copy backends never copy them.
pub(crate) const SECRET_DIRS: &[&str] = &[
    ".git",
    ".dial",
    ".codegraph",
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    "secrets",
    ".secrets",
    "credentials",
];
/// Build outputs, caches, and installed dependencies. The copy backends exclude them from their
/// change sets; overlay leaves them visible and ignores changes made under them.
pub(crate) const DEPENDENCY_DIRS: &[&str] = &[
    "target",
    "build",
    "dist",
    ".cache",
    ".venv",
    "venv",
    ".tox",
    "__pycache__",
    "node_modules",
    "vendor",
];
/// Root-only build caches and tool scratch; unlike these, `bin/` often contains real source.
const ROOT_GENERATED_DIRS: &[&str] = &[".cargo-work", crate::tools::SCRATCH_DIR];
const EXCLUDED_FILES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "credentials.json",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
    "id_rsa",
    "id_ed25519",
];

/// Copy a source tree into a unique temporary directory, omitting generated/state directories.
/// Symlinks are materialized only when their canonical targets remain inside the source root.
pub fn clone_project(source: &Path) -> Result<tempfile::TempDir> {
    copy_project_with(source, crate::platform::fs::copy_on_write)
}

/// Copy the project file by file with plain reads and writes, never reflinks. The `copy`
/// isolation backend uses it so its behavior does not depend on the filesystem.
pub(crate) fn copy_project_plain(source: &Path) -> Result<tempfile::TempDir> {
    copy_project_with(source, plain_copy_file)
}

/// Copies one regular file. Returns the standard library's error unchanged.
type CopyFile = fn(&Path, &Path) -> std::io::Result<()>;

fn plain_copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::copy(from, to).map(|_| ())
}

fn copy_project_with(source: &Path, copy_file: CopyFile) -> Result<tempfile::TempDir> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("resolving benchmark source {}", source.display()))?;
    ensure!(source.is_dir(), "benchmark source must be a directory");
    let destination = tempfile::tempdir().context("creating benchmark workspace")?;
    let mut ancestors = std::collections::BTreeSet::new();
    copy_directory(
        &source,
        destination.path(),
        &source,
        &mut ancestors,
        copy_file,
    )?;
    Ok(destination)
}

/// `ancestors` holds the canonical directories being copied on the current path. It cuts symlink
/// cycles only: a directory reached through a second name is copied again, so no name is left
/// empty because another name was visited first.
fn copy_directory(
    source: &Path,
    dest: &Path,
    root: &Path,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
    copy_file: CopyFile,
) -> Result<()> {
    let canonical = fs::canonicalize(source)?;
    ensure!(
        canonical.starts_with(root),
        "source symlink escapes project root"
    );
    if !ancestors.insert(canonical.clone()) {
        return Ok(());
    }
    let copied = copy_entries(source, dest, root, ancestors, copy_file);
    ancestors.remove(&canonical);
    copied
}

fn copy_entries(
    source: &Path,
    dest: &Path,
    root: &Path,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
    copy_file: CopyFile,
) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        if is_excluded_source_path(root, &from) {
            continue;
        }
        let name = entry.file_name();
        let to = dest.join(&name);
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            let target = fs::canonicalize(&from)
                .with_context(|| format!("resolving symlink {}", from.display()))?;
            ensure!(
                target.starts_with(root),
                "source symlink escapes project root: {}",
                from.display()
            );
            if is_excluded_source_path(root, &target) {
                continue;
            }
            if target.is_dir() {
                fs::create_dir_all(&to)?;
                copy_directory(&target, &to, root, ancestors, copy_file)?;
            } else {
                copy_file(&target, &to).with_context(|| format!("copying {}", from.display()))?;
            }
        } else if metadata.is_dir() {
            fs::create_dir(&to)?;
            copy_directory(&from, &to, root, ancestors, copy_file)?;
        } else if metadata.is_file() {
            copy_file(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn is_secret_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if EXCLUDED_FILES.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with(".env.")
        && !matches!(
            lower.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        )
    {
        return true;
    }
    [".pem", ".p12", ".pfx"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Whether `path` is a secret or private entry under `root`: a secret directory, a secret file, or
/// anything outside `root` (which fails closed). Overlay masks these entries.
pub(crate) fn is_secret_source_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    relative.components().any(|component| {
        let Component::Normal(name) = component else {
            return true;
        };
        let name = name.to_string_lossy();
        SECRET_DIRS.contains(&name.as_ref()) || is_secret_file(&name)
    })
}

/// Whether `path` lies in a build output, cache, or installed-dependency directory under `root`,
/// or is one of the root-level generated directories. Overlay leaves these visible and ignores
/// changes made under them.
pub(crate) fn is_dependency_source_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().any(|component| {
        matches!(component, Component::Normal(name) if DEPENDENCY_DIRS.contains(&name.to_string_lossy().as_ref()))
    }) || matches!(
        relative.components().next(),
        Some(Component::Normal(name)) if ROOT_GENERATED_DIRS.contains(&name.to_string_lossy().as_ref())
    ) || ignored_bin_output(root, relative)
}

/// Require the project's own ignore rule for `/bin/` and preserve tracked source scripts.
fn ignored_bin_output(root: &Path, relative: &Path) -> bool {
    if !relative.starts_with("bin") {
        return false;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    let Ok(ignore) = builder.build() else {
        return false;
    };
    if !ignore.matched(root.join("bin"), true).is_ignore() {
        return false;
    }
    // A tracked descendant keeps the directory visible; individual ignored outputs below it
    // can still be excluded. This local query runs no hooks and reads no file contents.
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(format!(":(literal){}", relative.display()))
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    !tracked.is_ok_and(|status| status.success())
}

/// Whether the copy backends leave `path` out of their copies and change sets: a secret entry or
/// a dependency directory.
pub(crate) fn is_excluded_source_path(root: &Path, path: &Path) -> bool {
    is_secret_source_path(root, path) || is_dependency_source_path(root, path)
}

#[cfg(test)]
mod tests;
