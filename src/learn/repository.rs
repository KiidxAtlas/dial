//! Immutable, operator-registered repository exercises, separate from the live checkout.
//! The agent never chooses its success labels or sees sealed verification files.
use super::{curriculum::Task, verify::Check};
use crate::{config::types::Config, tools::Workspace};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Specification {
    pub id: String,
    pub prompt: String,
    /// train or evaluation; immutable once registered.
    pub split: String,
    pub files: Vec<String>,
    pub verification_files: Vec<String>,
    pub command: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub spec: Specification,
    pub files: BTreeMap<String, String>,
    pub verification: BTreeMap<String, String>,
    pub digest: String,
}
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path
            .split('/')
            .any(|c| matches!(c, ".git" | ".env" | ".dial" | ".aws" | ".ssh"))
}
fn capture(root: &Path, paths: &[String]) -> Result<BTreeMap<String, String>> {
    ensure!(
        !paths.is_empty() && paths.len() <= 128,
        "snapshot requires 1..128 explicit paths"
    );
    let mut files = BTreeMap::new();
    let mut total = 0;
    for path in paths {
        ensure!(
            valid_path(path),
            "snapshot path is not a permitted relative source path"
        );
        let mut probe = root.to_path_buf();
        for part in Path::new(path).components() {
            probe.push(part);
            ensure!(
                !fs::symlink_metadata(&probe)?.file_type().is_symlink(),
                "snapshot cannot follow symlinks"
            );
        }
        ensure!(
            fs::metadata(&probe)?.len() <= 256 * 1024,
            "snapshot file exceeds 256 KiB"
        );
        let content = fs::read_to_string(probe)?;
        total += content.len();
        ensure!(total <= 2 * 1024 * 1024, "snapshot exceeds 2 MiB");
        ensure!(
            files.insert(path.clone(), content).is_none(),
            "duplicate snapshot path"
        );
    }
    Ok(files)
}
impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.spec.id.is_empty()
                && self.spec.id.len() <= 64
                && self
                    .spec
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "invalid exercise ID"
        );
        ensure!(
            matches!(self.spec.split.as_str(), "train" | "evaluation"),
            "split must be train or evaluation"
        );
        ensure!(
            !self.spec.prompt.trim().is_empty() && self.spec.prompt.len() <= 16384,
            "invalid exercise prompt"
        );
        ensure!(
            !self.spec.command.trim().is_empty() && self.spec.command.len() <= 8192,
            "invalid verification command"
        );
        ensure!(
            !self.files.is_empty() && !self.verification.is_empty(),
            "source and sealed verification files are required"
        );
        ensure!(
            self.files
                .keys()
                .chain(self.verification.keys())
                .all(|p| valid_path(p)),
            "invalid snapshot path"
        );
        for map in [&self.files, &self.verification] {
            ensure!(
                map.len() <= 128
                    && map.values().all(|v| v.len() <= 256 * 1024)
                    && map.values().map(String::len).sum::<usize>() <= 2 * 1024 * 1024,
                "snapshot exceeds size limits"
            );
        }
        for paths in [&self.spec.files, &self.spec.verification_files] {
            ensure!(
                !paths.is_empty()
                    && paths.len() <= 128
                    && paths.iter().all(|p| valid_path(p))
                    && paths
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == paths.len(),
                "invalid manifest paths"
            );
        }
        ensure!(
            self.files.keys().all(|p| self.spec.files.contains(p))
                && self.verification.keys().eq(self
                    .spec
                    .verification_files
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()),
            "snapshot paths differ from manifest"
        );
        ensure!(
            self.files
                .keys()
                .all(|p| !self.verification.contains_key(p)),
            "verification must be separate from source"
        );
        ensure!(
            self.digest == fingerprint(&self.files, &self.verification),
            "snapshot digest mismatch"
        );
        Ok(())
    }
    pub fn task(&self) -> Task {
        Task {
            family: "repository",
            prompt: self.spec.prompt.clone(),
            files: self.files.clone(),
            check: Check::Repository {
                files: self.verification.clone(),
                command: self.spec.command.clone(),
            },
        }
    }
}
fn fingerprint(files: &BTreeMap<String, String>, checks: &BTreeMap<String, String>) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(files, checks)).expect("string maps serialize"))
    )
}
pub fn load(config: &Config) -> Result<Vec<Snapshot>> {
    let dir = config.state_dir.join("repository-tasks");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        ensure!(
            snapshots.len() < 128,
            "too many registered repository tasks"
        );
        ensure!(
            entry.file_type()?.is_file() && entry.metadata()?.len() <= 5 * 1024 * 1024,
            "invalid repository snapshot file"
        );
        let snapshot: Snapshot = serde_json::from_slice(&fs::read(entry.path())?)?;
        snapshot.validate()?;
        snapshots.push(snapshot);
    }
    snapshots.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    for (i, a) in snapshots.iter().enumerate() {
        ensure!(
            !snapshots[..i].iter().any(|b| b.digest == a.digest
                || (b.spec.split != a.spec.split && b.files == a.files)
                || b.spec.prompt.trim() == a.spec.prompt.trim()),
            "duplicate exercise or cross-split leakage"
        );
    }
    Ok(snapshots)
}
fn read_manifest(manifest: &Path) -> Result<Specification> {
    ensure!(
        fs::metadata(manifest)?.len() <= 65536,
        "manifest exceeds 64 KiB"
    );
    Ok(serde_json::from_slice(&fs::read(manifest)?)?)
}
fn capture_manifest(config: &Config, manifest: &Path) -> Result<Snapshot> {
    let spec = read_manifest(manifest)?;
    let snapshot = Snapshot {
        files: capture(&config.project_root, &spec.files)?,
        verification: capture(&config.project_root, &spec.verification_files)?,
        spec,
        digest: String::new(),
    };
    let snapshot = Snapshot {
        digest: fingerprint(&snapshot.files, &snapshot.verification),
        ..snapshot
    };
    snapshot.validate()?;
    Ok(snapshot)
}

pub fn register(config: &Config, manifest: &Path) -> Result<Snapshot> {
    persist(config, capture_manifest(config, manifest)?)
}
fn persist(config: &Config, snapshot: Snapshot) -> Result<Snapshot> {
    snapshot.validate()?;
    let existing = load(config)?;
    ensure!(
        !existing.iter().any(|s| s.spec.id == snapshot.spec.id
            || s.digest == snapshot.digest
            || (s.spec.split != snapshot.spec.split && s.files == snapshot.files)
            || s.spec.prompt.trim() == snapshot.spec.prompt.trim()),
        "exercise already registered; do not reuse training material for evaluation"
    );
    let dir = config.state_dir.join("repository-tasks");
    crate::storage::settings_store::private_dir(&dir)?;
    crate::storage::settings_store::write_private(
        &dir.join(format!("{}.json", snapshot.spec.id)),
        &serde_json::to_vec(&snapshot)?,
    )?;
    Ok(snapshot)
}

/// Import retained failed-state bytes, not the already repaired live checkout. Independent
/// operator checks must reproduce failure-before/pass-after before the snapshot is registered.
pub fn register_repair(
    config: &Config,
    store: &crate::storage::store::Store,
    manifest: &Path,
    trajectory: i64,
) -> Result<Snapshot> {
    use crate::agent::repair_evidence::{CheckKind, RepairRecord};
    let spec = read_manifest(manifest)?;
    ensure!(
        spec.split == "train",
        "observed repairs are training data, not unseen evaluation tasks"
    );
    let records = store.recent_executable_repairs(&config.project_root.to_string_lossy(), 1024)?;
    let matching: Vec<_> = records
        .into_iter()
        .filter(|(id, _)| *id == trajectory)
        .collect();
    ensure!(
        matching.len() == 1,
        "trajectory must identify exactly one retained repair; use a task manifest for ambiguous repairs"
    );
    let record: RepairRecord = serde_json::from_value(matching.into_iter().next().unwrap().1)?;
    ensure!(
        record.valid() && record.kind == CheckKind::Behavioral,
        "repair lacks behavioral failure/change/pass evidence"
    );
    // Changed files come exclusively from retained snapshots, including deleted files.
    let unchanged: Vec<_> = spec
        .files
        .iter()
        .filter(|path| !record.changes.iter().any(|c| &c.path == *path))
        .cloned()
        .collect();
    let files = if unchanged.is_empty() {
        BTreeMap::new()
    } else {
        capture(&config.project_root, &unchanged)?
    };
    let mut snapshot = Snapshot {
        files,
        verification: capture(&config.project_root, &spec.verification_files)?,
        spec,
        digest: String::new(),
    };
    let layout = crate::storage::paths::Layout::with_user_dir(
        &config.project_root,
        config.user_dir.clone(),
    )?;
    let root = layout.checkpoints_dir().canonicalize()?;
    let read_snapshot = |reference: &Option<String>,
                         digest: &Option<String>|
     -> Result<Option<String>> {
        let (Some(reference), Some(digest)) = (reference, digest) else {
            return Ok(None);
        };
        let path = Path::new(reference).canonicalize()?;
        ensure!(
            path.starts_with(&root) && path.file_name().is_some_and(|name| name == digest.as_str()),
            "repair source outside project checkpoint storage"
        );
        ensure!(
            fs::metadata(&path)?.len() <= 256 * 1024,
            "repair source exceeds snapshot limit"
        );
        let bytes = fs::read(path)?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == *digest,
            "repair source digest mismatch"
        );
        Ok(Some(String::from_utf8(bytes)?))
    };
    let mut repaired = snapshot.files.clone();
    for change in &record.changes {
        ensure!(
            snapshot.spec.files.contains(&change.path)
                && !snapshot.verification.contains_key(&change.path),
            "manifest must include every changed source; changing verifier files is not an admissible repair"
        );
        match read_snapshot(&change.before_ref, &change.before_sha256)? {
            Some(content) => {
                snapshot.files.insert(change.path.clone(), content);
            }
            None => {
                snapshot.files.remove(&change.path);
            }
        }
        match read_snapshot(&change.after_ref, &change.after_sha256)? {
            Some(content) => {
                repaired.insert(change.path.clone(), content);
            }
            None => {
                repaired.remove(&change.path);
            }
        }
    }
    let replay = |files: &BTreeMap<String, String>| -> Result<bool> {
        let workspace = tempfile::tempdir()?;
        for (path, content) in files {
            ensure!(valid_path(path), "invalid replay path");
            let target = workspace.path().join(path);
            fs::create_dir_all(target.parent().context("source path has no parent")?)?;
            fs::write(target, content)?;
        }
        Ok(verify(
            workspace.path(),
            &snapshot.verification,
            &snapshot.spec.command,
        ))
    };
    ensure!(
        !replay(&snapshot.files)?,
        "independent checks do not reproduce the original failure"
    );
    ensure!(
        replay(&repaired)?,
        "independent checks do not verify the recorded correction"
    );
    snapshot.digest = fingerprint(&snapshot.files, &snapshot.verification);
    persist(config, snapshot)
}

/// Restore sealed tests only after the agent finishes. A solver-written test cannot replace them.
pub fn verify(root: &Path, files: &BTreeMap<String, String>, command: &str) -> bool {
    let run = || -> Result<bool> {
        let workspace = Workspace::new(root)?;
        for (path, content) in files {
            ensure!(valid_path(path), "invalid verification path");
            // Refuse all symlink components before writing any verification bytes.
            let mut probe = root.to_path_buf();
            for part in Path::new(path).components() {
                probe.push(part);
                ensure!(
                    !fs::symlink_metadata(&probe).is_ok_and(|m| m.file_type().is_symlink()),
                    "verification path contains a symlink"
                );
            }
            if let Some(parent) = probe.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(probe, content)?;
        }
        let _ = workspace;
        Ok(Check::Command {
            command: command.into(),
            expect_stdout: None,
            expect_exit: 0,
        }
        .verify(root, ""))
    };
    run().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_are_immutable_and_cannot_cross_splits() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.py"), "answer=41").unwrap();
        fs::write(
            root.path().join("test.py"),
            "from source import answer; assert answer==42",
        )
        .unwrap();
        let manifest = root.path().join("task.json");
        let mut spec = Specification {
            id: "repair".into(),
            prompt: "Fix the answer".into(),
            split: "train".into(),
            files: vec!["source.py".into()],
            verification_files: vec!["test.py".into()],
            command: "python3 test.py".into(),
        };
        fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
        let config = Config {
            project_root: root.path().into(),
            state_dir: state.path().into(),
            ..Config::default()
        };
        register(&config, &manifest).unwrap();
        spec.id = "leak".into();
        spec.split = "evaluation".into();
        fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
        assert!(register(&config, &manifest).is_err());
        fs::write(root.path().join("source.py"), "answer=42").unwrap();
        assert_eq!(load(&config).unwrap()[0].files["source.py"], "answer=41");
        spec.files = vec!["../escape".into()];
        fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
        assert!(register(&config, &manifest).is_err());
    }
    #[test]
    fn retained_deletion_replays_before_and_after_independent_checks() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        use crate::agent::repair_evidence::{CheckKind, CheckRun, FileChange, Tracker};
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let config = Config {
            project_root: project.path().canonicalize().unwrap(),
            user_dir: home.path().into(),
            state_dir: home.path().join("model"),
            ..Config::default()
        };
        let layout = crate::storage::paths::Layout::with_user_dir(
            &config.project_root,
            config.user_dir.clone(),
        )
        .unwrap();
        let retained = layout.checkpoints_dir().join("repair-evidence");
        fs::create_dir_all(&retained).unwrap();
        let content = "obsolete";
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        let reference = retained.join(&digest);
        fs::write(&reference, content).unwrap();
        fs::write(project.path().join("keep.txt"), "keep").unwrap();
        fs::write(
            project.path().join("check.sh"),
            "test ! -e obsolete.txt && test -f keep.txt",
        )
        .unwrap();
        let spec = Specification {
            id: "deletion".into(),
            prompt: "Remove obsolete.txt and preserve keep.txt".into(),
            split: "train".into(),
            files: vec!["keep.txt".into(), "obsolete.txt".into()],
            verification_files: vec!["check.sh".into()],
            command: "sh check.sh".into(),
        };
        let manifest = project.path().join("exercise.json");
        fs::write(&manifest, serde_json::to_vec(&spec).unwrap()).unwrap();
        let store =
            crate::storage::store::Store::open(&config.state_dir, "model", "case", &config.harness)
                .unwrap();
        let version = store.active().unwrap().id;
        let trajectory = store
            .begin_trajectory(version, "coding", "repair", "remove obsolete file")
            .unwrap();
        let run = |sequence, exit_code| CheckRun {
            command: "sh check.sh".into(),
            cwd: config.project_root.to_string_lossy().into(),
            objective: "obsolete file absent".into(),
            kind: CheckKind::Behavioral,
            tool_call_id: format!("check-{sequence}"),
            evidence_ref: format!("artifact://{sequence}"),
            exit_code: Some(exit_code),
            sequence,
        };
        let mut tracker = Tracker::default();
        tracker.observe(run(1, 1), vec![]);
        let record = tracker
            .observe(
                run(2, 0),
                vec![FileChange {
                    path: "obsolete.txt".into(),
                    before_sha256: Some(digest),
                    before_ref: Some(reference.to_string_lossy().into()),
                    after_sha256: None,
                    after_ref: None,
                }],
            )
            .unwrap();
        store
            .executable_repair(trajectory, version, &serde_json::to_value(record).unwrap())
            .unwrap();
        let snapshot = register_repair(&config, &store, &manifest, trajectory).unwrap();
        assert_eq!(snapshot.files["obsolete.txt"], "obsolete");
        assert!(!project.path().join("obsolete.txt").exists());
        assert!(store.verified_successes(100.0, 100).unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn solver_cannot_replace_sealed_tests_or_follow_symlinks() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let checks = BTreeMap::from([(
            "test.py".into(),
            "from source import answer; assert answer==42".into(),
        )]);
        fs::write(root.path().join("source.py"), "answer=41").unwrap();
        fs::write(root.path().join("test.py"), "pass").unwrap();
        assert!(!verify(root.path(), &checks, "python3 -B test.py"));
        fs::write(root.path().join("source.py"), "answer=42").unwrap();
        assert!(verify(root.path(), &checks, "python3 -B test.py"));
        fs::remove_file(root.path().join("test.py")).unwrap();
        std::os::unix::fs::symlink("source.py", root.path().join("test.py")).unwrap();
        assert!(!verify(root.path(), &checks, "python3 -B test.py"));
    }
}
