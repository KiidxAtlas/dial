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
pub const SECRET_DIRS: &[&str] = &[
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
pub const DEPENDENCY_DIRS: &[&str] = &[
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
/// Per-workspace scratch directory exported as TMPDIR for bash; walks skip it.
pub const SCRATCH_DIR: &str = ".tmp";

/// Root-only build caches and tool scratch; unlike these, `bin/` often contains real source.
pub const ROOT_GENERATED_DIRS: &[&str] = &[".cargo-work", SCRATCH_DIR];
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

/// Gitignored, untracked paths of a project, relative to its root. Isolated workers leave them
/// out: they are local data (benchmark runs, downloads, private notes), not source to merge, and
/// can be far larger than the isolation limits. Tracked files under an ignored pattern are not
/// listed, and dependency directories keep their own handling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoredPaths(std::collections::BTreeSet<PathBuf>);

impl IgnoredPaths {
    /// Asks `git ls-files` (no hooks, no fsmonitor) at `root`. Outside a Git work tree, when Git
    /// is unavailable or fails, or when `root` itself is ignored, nothing is treated as ignored.
    pub fn of(root: &Path) -> Self {
        let child = crate::platform::process::spawn(
            std::process::Command::new("git")
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "ls-files",
                    "-z",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                ])
                .current_dir(root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null()),
        );
        match child.and_then(|child| child.wait_with_output()) {
            Ok(output) if output.status.success() => Self::parse(&output.stdout),
            _ => Self::default(),
        }
    }

    /// Parses `ls-files -z --directory` output. Non-UTF-8 names are kept as ordinary source.
    pub fn parse(output: &[u8]) -> Self {
        let mut paths = std::collections::BTreeSet::new();
        for item in output
            .split(|byte| *byte == 0)
            .filter(|item| !item.is_empty())
        {
            let Ok(text) = std::str::from_utf8(item) else {
                continue;
            };
            let path = PathBuf::from(text.trim_end_matches('/'));
            if path.as_os_str().is_empty() || path == Path::new(".") {
                return Self::default();
            }
            paths.insert(path);
        }
        Self(paths)
    }

    /// Whether `relative` is an ignored path or lies under an ignored directory.
    pub fn covers(&self, relative: &Path) -> bool {
        !self.0.is_empty()
            && relative
                .ancestors()
                .any(|ancestor| self.0.contains(ancestor))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Copy a source tree into a unique temporary directory, omitting generated/state directories.
/// Symlinks are materialized only when their canonical targets remain inside the source root.
pub fn clone_project(source: &Path) -> Result<tempfile::TempDir> {
    copy_project_with(
        source,
        crate::platform::fs::copy_on_write,
        &IgnoredPaths::default(),
    )
}

/// `clone_project` without the `ignored` paths, for an isolated worker's copy.
pub fn clone_project_except(source: &Path, ignored: &IgnoredPaths) -> Result<tempfile::TempDir> {
    copy_project_with(source, crate::platform::fs::copy_on_write, ignored)
}

/// Freeze a checkout's source for a benchmark run: `clone_project` without its gitignored,
/// untracked local data (benchmark runs, virtualenvs, downloads). That data is not source, can be
/// far larger than the source itself, and may hold symlinks that escape the checkout.
pub fn freeze_source(root: &Path) -> Result<tempfile::TempDir> {
    clone_project_except(root, &IgnoredPaths::of(root))
}

/// Copy the project file by file with plain reads and writes, never reflinks. The `copy`
/// isolation backend uses it so its behavior does not depend on the filesystem.
pub fn copy_project_plain(source: &Path) -> Result<tempfile::TempDir> {
    copy_project_with(source, plain_copy_file, &IgnoredPaths::default())
}

/// `copy_project_plain` without the `ignored` paths, for an isolated worker's copy.
pub fn copy_project_plain_except(
    source: &Path,
    ignored: &IgnoredPaths,
) -> Result<tempfile::TempDir> {
    copy_project_with(source, plain_copy_file, ignored)
}

/// Copies one regular file. Returns the standard library's error unchanged.
type CopyFile = fn(&Path, &Path) -> std::io::Result<()>;

fn plain_copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::copy(from, to).map(|_| ())
}

fn copy_project_with(
    source: &Path,
    copy_file: CopyFile,
    ignored: &IgnoredPaths,
) -> Result<tempfile::TempDir> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("resolving benchmark source {}", source.display()))?;
    ensure!(source.is_dir(), "benchmark source must be a directory");
    let destination = tempfile::tempdir().context("creating benchmark workspace")?;
    let mut ancestors = std::collections::BTreeSet::new();
    let copy = Copy {
        root: &source,
        copy_file,
        ignored,
    };
    copy_directory(&source, destination.path(), &copy, &mut ancestors)?;
    Ok(destination)
}

/// What one project copy reads from and leaves out.
struct Copy<'a> {
    root: &'a Path,
    copy_file: CopyFile,
    ignored: &'a IgnoredPaths,
}

impl Copy<'_> {
    fn skips(&self, path: &Path) -> bool {
        is_excluded_source_path(self.root, path)
            || path
                .strip_prefix(self.root)
                .is_ok_and(|relative| self.ignored.covers(relative))
    }
}

/// `ancestors` holds the canonical directories being copied on the current path. It cuts symlink
/// cycles only: a directory reached through a second name is copied again, so no name is left
/// empty because another name was visited first.
fn copy_directory(
    source: &Path,
    dest: &Path,
    copy: &Copy<'_>,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    let canonical = fs::canonicalize(source)?;
    ensure!(
        canonical.starts_with(copy.root),
        "source symlink escapes project root"
    );
    if !ancestors.insert(canonical.clone()) {
        return Ok(());
    }
    let copied = copy_entries(source, dest, copy, ancestors);
    ancestors.remove(&canonical);
    copied
}

fn copy_entries(
    source: &Path,
    dest: &Path,
    copy: &Copy<'_>,
    ancestors: &mut std::collections::BTreeSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(source).with_context(|| format!("reading {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        if copy.skips(&from) {
            continue;
        }
        let name = entry.file_name();
        let to = dest.join(&name);
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            let target = fs::canonicalize(&from)
                .with_context(|| format!("resolving symlink {}", from.display()))?;
            ensure!(
                target.starts_with(copy.root),
                "source symlink escapes project root: {}",
                from.display()
            );
            if copy.skips(&target) {
                continue;
            }
            if target.is_dir() {
                fs::create_dir_all(&to)?;
                copy_directory(&target, &to, copy, ancestors)?;
            } else {
                (copy.copy_file)(&target, &to)
                    .with_context(|| format!("copying {}", from.display()))?;
            }
        } else if metadata.is_dir() {
            fs::create_dir(&to)?;
            copy_directory(&from, &to, copy, ancestors)?;
        } else if metadata.is_file() {
            (copy.copy_file)(&from, &to).with_context(|| format!("copying {}", from.display()))?;
        }
    }
    Ok(())
}

pub fn is_secret_file(name: &str) -> bool {
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
pub fn is_secret_source_path(root: &Path, path: &Path) -> bool {
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
pub fn is_dependency_source_path(root: &Path, path: &Path) -> bool {
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
    let tracked = crate::platform::process::spawn(
        std::process::Command::new("git")
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(format!(":(literal){}", relative.display()))
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )
    .and_then(|mut child| child.wait());
    !tracked.is_ok_and(|status| status.success())
}

/// Whether the copy backends leave `path` out of their copies and change sets: a secret entry or
/// a dependency directory.
pub fn is_excluded_source_path(root: &Path, path: &Path) -> bool {
    is_secret_source_path(root, path) || is_dependency_source_path(root, path)
}

#[cfg(test)]
mod tests;
