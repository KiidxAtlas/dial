//! Explicit, copy-based imports; no network access or script execution.
use super::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MANIFEST: &str = ".dial-import.json";
const MAX_TOTAL: usize = 8 * 1024 * 1024;
type Files = BTreeMap<PathBuf, Vec<u8>>;

// Serialize installers targeting the same root. Dry runs remain entirely read-only.
fn lock_root(root: &Path) -> Result<std::fs::File> {
    fs::create_dir_all(root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join(".import.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file).context("another skill import is running")?;
    Ok(file)
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
        vec![source.clone()]
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
        Some(lock_root(&root)?)
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
mod tests {
    use super::*;
    #[test]
    fn import_preview_update_and_local_edit_protection() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let source = sources.path().join("review");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join(SKILL_FILE),
            "---\nname: review\ndescription: Review code\n---\nReview carefully.",
        )
        .unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let run_import = |dry, update| {
            run(&layout, &source, Scope::Project, dry, update)
                .unwrap()
                .join("\n")
        };
        assert!(run_import(true, false).contains("Would import"));
        assert!(!layout.project_dir.exists());
        assert!(run_import(false, false).contains("Imported"));
        assert_eq!(discover(&layout).len(), 1);
        assert!(run_import(false, false).contains("already installed"));
        fs::write(source.join("reference.md"), "new reference").unwrap();
        assert!(run_import(false, true).contains("Imported"));
        let installed = layout.skills_dirs()[0].join("review/reference.md");
        fs::write(&installed, "my edits").unwrap();
        assert!(run_import(false, true).contains("local changes"));
        assert_eq!(fs::read_to_string(installed).unwrap(), "my edits");
    }

    #[test]
    fn rejects_links_and_oversized_bundles() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let source = sources.path().join("review");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join(SKILL_FILE),
            "---\ndescription: Review\n---\nBody",
        )
        .unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        std::os::unix::fs::symlink("SKILL.md", source.join("link")).unwrap();
        assert!(
            run(&layout, &source, Scope::User, false, false).unwrap()[0].contains("symbolic link")
        );
        fs::remove_file(source.join("link")).unwrap();
        fs::write(source.join("large"), vec![0; MAX_READ_BYTES + 1]).unwrap();
        assert!(run(&layout, &source, Scope::User, false, false).unwrap()[0].contains("64 KiB"));
        assert!(discover(&layout).is_empty());
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[test]
    fn bulk_import_reports_invalid_skills_and_preserves_source_identity() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        for (name, declared) in [("review", "review"), ("bad", "different")] {
            let dir = sources.path().join(name);
            fs::create_dir(&dir).unwrap();
            fs::write(
                dir.join(SKILL_FILE),
                format!("---\nname: {declared}\ndescription: Test\n---\nBody"),
            )
            .unwrap();
        }
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let reports = run(&layout, sources.path(), Scope::User, false, false).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports[0].contains("Skipped"));
        assert_eq!(reports[1], "Imported review");
        let found = discover(&layout);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, Scope::User);
        let replacement = other.path().join("review");
        fs::create_dir(&replacement).unwrap();
        fs::copy(
            sources.path().join("review/SKILL.md"),
            replacement.join(SKILL_FILE),
        )
        .unwrap();
        assert!(
            run(&layout, &replacement, Scope::User, false, true).unwrap()[0]
                .contains("source differs")
        );
        std::os::unix::fs::symlink(home.path(), layout.project_dir.clone()).unwrap();
        assert!(run(&layout, sources.path(), Scope::Project, false, false).is_err());
    }
}
