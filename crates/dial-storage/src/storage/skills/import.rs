//! Explicit, copy-based imports; no network access or script execution.
use super::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MANIFEST: &str = ".dial-import.json";
const MAX_TOTAL: usize = 8 * 1024 * 1024;
type Files = BTreeMap<PathBuf, Vec<u8>>;

/// How long an installer waits for another one on the same root before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

// Serialize installers targeting the same root. Dry runs remain entirely read-only. A short wait
// covers a released lock whose descriptor a just-forked process still holds until its exec.
pub(super) fn lock_root(root: &Path, file: &str, label: &str) -> Result<std::fs::File> {
    fs::create_dir_all(root)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    let file = crate::platform::fs::open_nofollow(&options, &root.join(file))?;
    let started = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error())
                    && started.elapsed() < LOCK_WAIT =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => return Err(error).context(label.to_string()),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Origin {
    source: PathBuf,
    hashes: BTreeMap<PathBuf, String>,
}

fn hashes(files: &Files) -> BTreeMap<PathBuf, String> {
    files
        .iter()
        .map(|(p, bytes)| (p.clone(), format!("{:x}", Sha256::digest(bytes))))
        .collect()
}

fn collect(base: &Path, rel: &Path, files: &mut Files, total: &mut usize) -> Result<()> {
    ensure!(
        rel.components().count() <= MAX_LIST_DEPTH,
        "bundle is too deeply nested"
    );
    for entry in fs::read_dir(base.join(rel))? {
        let entry = entry?;
        let path = rel.join(entry.file_name());
        if path == Path::new(MANIFEST) {
            continue;
        }
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "symbolic link refused: {}",
            path.display()
        );
        if kind.is_dir() {
            collect(base, &path, files, total)?;
        } else {
            ensure!(kind.is_file(), "not a regular file: {}", path.display());
            ensure!(files.len() < MAX_LISTED_FILES, "too many bundled files");
            let (bytes, truncated) = read_head(&entry.path(), MAX_READ_BYTES)?;
            ensure!(!truncated, "file exceeds 64 KiB: {}", path.display());
            *total += bytes.len();
            ensure!(*total <= MAX_TOTAL, "bundle exceeds 8 MiB");
            files.insert(path, bytes);
        }
    }
    Ok(())
}

fn snapshot(dir: &Path) -> Result<Files> {
    ensure!(is_real_dir(dir), "not a real directory: {}", dir.display());
    let mut files = Files::new();
    collect(dir, Path::new(""), &mut files, &mut 0)?;
    Ok(files)
}

// Reject linked ancestors even when the final destination does not exist yet.
fn safe_destination(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if let Ok(meta) = fs::symlink_metadata(ancestor) {
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "unsafe destination: {}",
                ancestor.display()
            );
        }
    }
    Ok(())
}

pub fn run(
    layout: &Layout,
    source: &Path,
    scope: Scope,
    dry_run: bool,
    update: bool,
) -> Result<Vec<String>> {
    ensure!(is_real_dir(source), "source must be a real directory");
    let source = fs::canonicalize(source)?;
    let mut candidates = if source.join(SKILL_FILE).exists() {
        vec![source]
    } else {
        fs::read_dir(&source)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?
    };
    candidates.sort();
    ensure!(
        candidates.len() <= MAX_SKILLS_PER_DIR,
        "source has too many entries"
    );
    let root = scope.root(layout);
    safe_destination(&root)?;
    let _lock = if dry_run {
        None
    } else {
        Some(lock_root(
            &root,
            ".import.lock",
            "another skill import is running",
        )?)
    };
    let mut reports = Vec::new();
    for candidate in candidates {
        let Some(skill) = load_skill(&candidate, scope) else {
            reports.push(format!(
                "Skipped {}: not a compatible SKILL.md directory",
                candidate.display()
            ));
            continue;
        };
        let result = (|| -> Result<()> {
            let files = snapshot(&candidate)?;
            let destination = root.join(&skill.name);
            ensure!(
                !candidate.starts_with(&destination) && !destination.starts_with(&candidate),
                "source overlaps destination"
            );
            let exists = fs::symlink_metadata(&destination).is_ok();
            if exists {
                ensure!(
                    update,
                    "already installed; use --update for unchanged imports"
                );
                safe_destination(&destination)?;
                let (bytes, truncated) = read_head(&destination.join(MANIFEST), MAX_READ_BYTES)?;
                ensure!(!truncated, "origin manifest too large");
                let origin: Origin = serde_json::from_slice(&bytes)?;
                ensure!(
                    origin.source == candidate,
                    "source differs from recorded origin"
                );
                ensure!(
                    origin.hashes == hashes(&snapshot(&destination)?),
                    "local changes detected; refusing overwrite"
                );
            }
            if dry_run {
                return Ok(());
            }
            fs::create_dir_all(&root)?;
            let stage = tempfile::tempdir_in(&root)?;
            let staged = stage.path().join(&skill.name);
            fs::create_dir(&staged)?;
            for (rel, bytes) in &files {
                let path = staged.join(rel);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, bytes)?;
            }
            fs::write(
                staged.join(MANIFEST),
                serde_json::to_vec_pretty(&Origin {
                    source: candidate.clone(),
                    hashes: hashes(&files),
                })?,
            )?;
            let backup = stage.path().join(".previous");
            if exists {
                fs::rename(&destination, &backup)?;
            }
            if let Err(error) = fs::rename(&staged, &destination) {
                if exists {
                    fs::rename(&backup, &destination)
                        .context("restore previous skill after failed install")?;
                }
                return Err(error.into());
            }
            Ok(())
        })();
        reports.push(match result {
            Ok(()) => format!(
                "{} {}",
                if dry_run { "Would import" } else { "Imported" },
                skill.name
            ),
            Err(error) => format!("Skipped {}: {error:#}", skill.name),
        });
    }
    Ok(reports)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod source_tests;
