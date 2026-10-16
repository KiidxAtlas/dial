//! Private development dependencies; no hardlinks or writable links to the parent.
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

pub fn dependencies(source: &Path, dest: &Path, max_bytes: u64) -> Result<()> {
    let source = &source.canonicalize()?;
    let dest = &dest.canonicalize()?;
    let mut remaining = max_bytes;
    let mut files = 0usize;
    discover(source, source, dest, &mut remaining, &mut files)?;
    Ok(())
}

fn discover(
    root: &Path,
    source: &Path,
    dest: &Path,
    remaining: &mut u64,
    files: &mut usize,
) -> Result<()> {
    ensure!(
        *files < 500_000,
        "isolated dependency discovery entry limit exceeded"
    );
    *files += 1;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let target = dest.join(path.strip_prefix(root)?);
        if ["node_modules", ".venv", "venv", "vendor"]
            .iter()
            .any(|dep| name == *dep)
        {
            fs::create_dir_all(target.parent().context("dependency has no parent")?)?;
            copy_tree(&path, &target, root, dest, remaining, files, 0)?;
        } else if !crate::workspace_copy::is_excluded_source_path(root, &path)
            && entry.file_type()?.is_dir()
        {
            discover(root, &path, dest, remaining, files)?;
        }
    }
    Ok(())
}

fn copy_tree(
    source: &Path,
    dest: &Path,
    root: &Path,
    dest_root: &Path,
    remaining: &mut u64,
    files: &mut usize,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth < 128,
        "dependency links cycle or exceed nesting limit"
    );
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let target = source.canonicalize()?;
        // Virtual environments commonly link their interpreter outside the project. Copy the
        // executable itself; never retain a writable link to that installation.
        let interpreter = executable_interpreter(&target)
            && source.parent().is_some_and(|bin| {
                bin.ends_with("bin")
                    && bin.join("python").canonicalize().ok().as_ref() == Some(&target)
            });
        ensure!(
            (target.starts_with(root) || interpreter)
                && !target
                    .strip_prefix(root)
                    .unwrap_or(&target)
                    .components()
                    .any(|part| excluded(&part.as_os_str().to_string_lossy())),
            "dependency link escapes the project or names a secret: {}",
            source.display()
        );
        ensure!(
            !source.starts_with(&target),
            "cyclic dependency link: {}",
            source.display()
        );
        if target.starts_with(root) {
            ensure!(*files < 500_000, "isolated dependency entry limit exceeded");
            *files += 1;
            let rebased = dest_root.join(target.strip_prefix(root)?);
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(rebased, dest)?;
            }
            #[cfg(windows)]
            {
                if target.is_dir() {
                    std::os::windows::fs::symlink_dir(rebased, dest)?;
                } else {
                    std::os::windows::fs::symlink_file(rebased, dest)?;
                }
            }
            return Ok(());
        }
        return copy_tree(&target, dest, root, dest_root, remaining, files, depth + 1);
    }

    if meta.is_dir() {
        ensure!(*files < 500_000, "isolated dependency entry limit exceeded");
        *files += 1;
        fs::create_dir(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            // Apply secret exclusions even inside dependency trees; ignore only the dependency root.
            if excluded(&name.to_string_lossy()) {
                continue;
            }
            copy_tree(
                &entry.path(),
                &dest.join(name),
                root,
                dest_root,
                remaining,
                files,
                depth + 1,
            )?;
        }
    } else if meta.is_file() {
        ensure!(
            *files < 500_000 && meta.len() <= *remaining,
            "isolated dependency budget exceeded; increase task.isolation_dependency_bytes or disable dependency copying"
        );
        *remaining -= meta.len();
        *files += 1;
        crate::platform::fs::copy_on_write(source, dest)
            .with_context(|| format!("copying dependency {}", source.display()))?;
        relocate_environment_text(source, dest, root, dest_root, meta.len())?;
    }
    Ok(())
}

fn executable_interpreter(path: &Path) -> bool {
    use std::io::Read;
    if !path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("python"))
    {
        return false;
    }
    let Ok(mut file) =
        crate::platform::fs::open_nofollow(std::fs::OpenOptions::new().read(true), path)
    else {
        return false;
    };
    let mut magic = [0; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    // Native Python executables only: a repository cannot label an outside text file
    // "python" to have the copier follow it into the private workspace.
    magic == *b"\x7fELF"
        || magic[..2] == *b"MZ"
        || matches!(
            magic,
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
        )
}

fn excluded(name: &str) -> bool {
    crate::workspace_copy::is_secret_file(name)
        || [
            ".git",
            ".hg",
            ".svn",
            ".dial",
            ".codegraph",
            ".ssh",
            ".aws",
            ".gnupg",
            ".kube",
            "secrets",
            ".secrets",
            "credentials",
        ]
        .contains(&name)
}

fn relocate_environment_text(
    source: &Path,
    dest: &Path,
    root: &Path,
    dest_root: &Path,
    bytes: u64,
) -> Result<()> {
    let Ok(relative) = source.strip_prefix(root) else {
        return Ok(());
    };
    if bytes > 8 * 1024 * 1024
        || !relative
            .components()
            .any(|part| part.as_os_str() == ".venv" || part.as_os_str() == "venv")
    {
        return Ok(());
    }
    let launcher = source
        .parent()
        .is_some_and(|dir| dir.ends_with("bin") || dir.ends_with("Scripts"));
    if !launcher
        && source.file_name().is_none_or(|name| name != "pyvenv.cfg")
        && source.extension().is_none_or(|ext| ext != "pth")
    {
        return Ok(());
    }
    let Ok(text) = fs::read_to_string(dest) else {
        return Ok(());
    };
    let origin = root.to_string_lossy();
    let replacement = dest_root.to_string_lossy();
    let updated =
        super::subagents::rebase_text(&text, &origin, &replacement).unwrap_or_else(|| text.clone());
    // macOS's /tmp and /var aliases may have been used when creating the venv.
    #[cfg(target_os = "macos")]
    let updated = match origin.strip_prefix("/private/") {
        Some(alias) => super::subagents::rebase_text(&updated, &format!("/{alias}"), &replacement)
            .unwrap_or(updated),
        None => updated,
    };
    if updated != text {
        fs::write(dest, updated)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn copied_virtual_environment_executes_with_its_private_prefix() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let setup = std::process::Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(source.path().join(".venv"))
            .output()
            .unwrap();
        assert!(
            setup.status.success(),
            "{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        dependencies(source.path(), dest.path(), 128 * 1024 * 1024).unwrap();
        let run = std::process::Command::new(dest.path().join(".venv/bin/python"))
            .args([
                "-c",
                "import sys,pathlib; print(pathlib.Path(sys.prefix).resolve())",
            ])
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            dest.path()
                .join(".venv")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_links_stay_private_and_environment_launchers_are_relocated() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("node_modules/.bin")).unwrap();
        fs::create_dir_all(source.path().join("node_modules/pkg/bin")).unwrap();
        fs::write(source.path().join("node_modules/pkg/bin/cli"), "original").unwrap();
        std::os::unix::fs::symlink(
            "../pkg/bin/cli",
            source.path().join("node_modules/.bin/cli"),
        )
        .unwrap();
        fs::create_dir_all(source.path().join(".venv/bin")).unwrap();
        fs::write(
            source.path().join(".venv/bin/pytest"),
            format!("#!{}/.venv/bin/python\n", source.path().display()),
        )
        .unwrap();
        fs::create_dir_all(source.path().join("packages/app/node_modules/pkg")).unwrap();
        fs::write(
            source.path().join("packages/app/node_modules/pkg/index.js"),
            "nested",
        )
        .unwrap();
        dependencies(source.path(), dest.path(), 4096).unwrap();
        assert!(
            fs::symlink_metadata(dest.path().join("node_modules/.bin/cli"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(dest.path().join("node_modules/.bin/cli"), "private edit").unwrap();
        assert_eq!(
            fs::read_to_string(source.path().join("node_modules/pkg/bin/cli")).unwrap(),
            "original"
        );
        let launcher = fs::read_to_string(dest.path().join(".venv/bin/pytest")).unwrap();
        assert!(launcher.starts_with(&format!(
            "#!{}/.venv/bin/python",
            dest.path().canonicalize().unwrap().display()
        )));
        assert!(
            dest.path()
                .join("packages/app/node_modules/pkg/index.js")
                .exists()
        );
    }

    #[test]
    fn dependency_edits_do_not_touch_parent_and_secrets_are_omitted() {
        let source = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::create_dir(source.path().join("node_modules")).unwrap();
        fs::write(source.path().join("node_modules/pkg.js"), "original").unwrap();
        fs::write(source.path().join("node_modules/.env"), "secret").unwrap();
        dependencies(source.path(), dest.path(), 1024).unwrap();
        fs::write(dest.path().join("node_modules/pkg.js"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(source.path().join("node_modules/pkg.js")).unwrap(),
            "original"
        );
        assert!(!dest.path().join("node_modules/.env").exists());
        assert!(dependencies(source.path(), tempfile::tempdir().unwrap().path(), 1).is_err());
    }
}
