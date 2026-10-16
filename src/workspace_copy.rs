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
    ) || ignored_bin_output(root, &relative)
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
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;

    #[test]
    fn root_build_outputs_are_excluded_but_nested_sources_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!is_excluded_source_path(root, &root.join("bin/script")));
        fs::write(root.join(".gitignore"), "/bin/\n").unwrap();
        for excluded in [
            "bin",
            "bin/dial",
            "bin/dial.bak-20261015-131809",
            ".cargo-work/windows/rustc/lib/librustc_driver.dylib",
            ".tmp/dial-debug-dump-1",
        ] {
            assert!(
                is_excluded_source_path(root, &root.join(excluded)),
                "{excluded}"
            );
        }
        for kept in [
            "src/bin/tool.rs",
            "src/main.rs",
            "docs/bin/readme.md",
            "binary.txt",
            ".cargo/config.toml",
        ] {
            assert!(
                !is_excluded_source_path(root, &root.join(kept)),
                "{kept} is source and must be copied"
            );
        }
    }

    #[test]
    fn copies_omit_credentials_but_keep_templates() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join(".env"), "OPENAI_API_KEY=secret").unwrap();
        fs::write(
            source.path().join(".env.example"),
            "OPENAI_API_KEY=replace-me",
        )
        .unwrap();
        fs::write(source.path().join("private.pem"), "private key").unwrap();
        symlink(source.path().join(".env"), source.path().join("env_alias")).unwrap();
        fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
        let copied = clone_project(source.path()).unwrap();
        assert!(!copied.path().join(".env").exists());
        assert!(!copied.path().join("env_alias").exists());
        assert!(!copied.path().join("private.pem").exists());
        assert!(copied.path().join(".env.example").is_file());
        assert!(copied.path().join("main.rs").is_file());
    }

    #[test]
    fn plain_copy_applies_the_same_exclusions_as_clone() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
        for excluded in SECRET_DIRS.iter().chain(DEPENDENCY_DIRS) {
            fs::create_dir(source.path().join(excluded)).unwrap();
            fs::write(source.path().join(excluded).join("x"), "x").unwrap();
        }
        fs::write(source.path().join(".env"), "secret").unwrap();
        let copied = copy_project_plain(source.path()).unwrap();
        assert_eq!(
            fs::read_to_string(copied.path().join("main.rs")).unwrap(),
            "fn main() {}"
        );
        assert!(!copied.path().join(".env").exists());
        assert!(
            SECRET_DIRS
                .iter()
                .chain(DEPENDENCY_DIRS)
                .all(|d| !copied.path().join(d).exists())
        );
    }

    #[test]
    fn paths_outside_the_root_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert!(is_secret_source_path(
            root.path(),
            &outside.path().join("main.rs")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn directory_aliases_never_leave_the_real_directory_empty() {
        // Directory read order is unspecified, so the real directory must survive whichever of
        // its names the copier reaches first.
        let source = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("real")).unwrap();
        fs::write(source.path().join("real/a.txt"), "a").unwrap();
        for alias in 0..8 {
            symlink(
                source.path().join("real"),
                source.path().join(format!("alias-{alias}")),
            )
            .unwrap();
        }
        for copied in [
            clone_project(source.path()).unwrap(),
            copy_project_plain(source.path()).unwrap(),
        ] {
            assert_eq!(
                fs::read_to_string(copied.path().join("real/a.txt")).unwrap(),
                "a"
            );
            for alias in 0..8 {
                assert_eq!(
                    fs::read_to_string(copied.path().join(format!("alias-{alias}/a.txt"))).unwrap(),
                    "a"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_cycles_stop_and_external_targets_are_refused() {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("source.txt"), "source").unwrap();
        symlink(source.path(), source.path().join("cycle")).unwrap();
        for copied in [
            clone_project(source.path()).unwrap(),
            copy_project_plain(source.path()).unwrap(),
        ] {
            assert_eq!(
                fs::read_to_string(copied.path().join("source.txt")).unwrap(),
                "source"
            );
            assert_eq!(
                fs::read_dir(copied.path().join("cycle")).unwrap().count(),
                0
            );
        }
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private.txt"), "untouched").unwrap();
        symlink(outside.path(), source.path().join("escape")).unwrap();
        assert!(clone_project(source.path()).is_err());
        assert!(copy_project_plain(source.path()).is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("private.txt")).unwrap(),
            "untouched"
        );
    }
}
