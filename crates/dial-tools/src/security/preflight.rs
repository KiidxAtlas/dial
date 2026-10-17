//! Scan preflight: resolves the repository and target, digests the
//! tree, validates scope, knowledge bases and the output directory, and produces a fingerprinted
//! [`Plan`]. Git is read through the `git` binary (read-only subcommands only).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::contracts::{
    self, AccountRef, KnowledgeBaseRef, ModelRef, OutputPlan, Plan, TargetKind, TargetPlan,
};

/// Target selection as requested by the user or tool.
#[derive(Debug, Clone)]
pub enum TargetRequest {
    Repository {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    ScopedPath {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    RefDiff {
        base: String,
        head: String,
        include: Vec<String>,
        exclude: Vec<String>,
    },
    WorkingTree {
        include: Vec<String>,
        exclude: Vec<String>,
    },
}

/// Inputs to a plan. `account` is the signed-in ChatGPT reviewer account's id.
pub struct PlanRequest<'a> {
    pub cwd: &'a Path,
    pub target: TargetRequest,
    pub knowledge_bases: Vec<String>,
    pub output_root: &'a Path,
    pub archive_existing: bool,
    pub model: ModelRef,
    pub account: AccountRef,
    pub config_material: Value,
    pub workflow_fingerprint: String,
}

/// Runs `git -C <cwd> <args>` and returns trimmed stdout; `None` on a non-zero exit.
fn git(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .context("running git")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

fn git_required(cwd: &Path, args: &[&str]) -> Result<String> {
    git(cwd, args)?.with_context(|| format!("git {} failed", args.join(" ")))
}

pub fn repository_root(cwd: &Path) -> Result<Option<String>> {
    Ok(git(cwd, &["rev-parse", "--show-toplevel"])?
        .map(|s| s.trim_end_matches(['\r', '\n']).to_owned()))
}

fn head_sha(cwd: &Path) -> Result<Option<String>> {
    Ok(git(cwd, &["rev-parse", "--verify", "-q", "HEAD"])?.map(|s| s.trim().to_owned()))
}

fn resolve_ref(cwd: &Path, name: &str) -> Result<Option<String>> {
    let spec = format!("{name}^{{commit}}");
    Ok(git(cwd, &["rev-parse", "--verify", "-q", &spec])?.map(|s| s.trim().to_owned()))
}

/// Tracked and untracked (not ignored) files, NUL-separated in git output.
fn list_files(cwd: &Path) -> Result<Vec<String>> {
    let tracked = git_required(cwd, &["ls-files", "-z"])?;
    let untracked = git_required(cwd, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Slash-normalized, repository-relative path with no traversal.
pub fn normalize_relative(input: &str) -> Result<String> {
    ensure!(
        !input.contains('\0'),
        "Security scope path contains a null byte: {input}"
    );
    let slashed = input.replace('\\', "/");
    if slashed.is_empty() {
        return Ok(String::new());
    }
    let raw_has_parent = slashed.split('/').any(|segment| segment == "..");
    let bytes = slashed.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let mut parts: Vec<&str> = Vec::new();
    for segment in slashed.split('/') {
        match segment {
            "" | "." => {}
            other => parts.push(other),
        }
    }
    let normalized = if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    };
    ensure!(
        !(raw_has_parent
            || normalized == ".."
            || normalized.starts_with("../")
            || slashed.starts_with('/')
            || drive),
        "Security scope path must be repository-relative: {input}"
    );
    Ok(normalized)
}

fn normalize_scope(values: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for value in values {
        let normalized = normalize_relative(value)?;
        if !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out.sort();
    Ok(out)
}

fn scope_contains(candidate: &str, path: &str) -> bool {
    candidate.is_empty()
        || candidate == "."
        || path == candidate
        || path.starts_with(&format!("{candidate}/"))
}

/// Whether `relative` is in scope: included by any include root (or none) and not excluded.
pub fn path_in_scope(relative: &str, include: &[String], exclude: &[String]) -> bool {
    let included = include.is_empty() || include.iter().any(|c| scope_contains(c, relative));
    let excluded = exclude.iter().any(|c| scope_contains(c, relative));
    included && !excluded
}

/// Lexical containment: `candidate` equals `root` or lies beneath it.
pub fn path_within(candidate: &Path, root: &Path) -> bool {
    candidate.starts_with(root)
}

fn validate_scope_paths(root: &Path, paths: &[String]) -> Result<()> {
    for relative in paths {
        if relative.is_empty() {
            continue;
        }
        let absolute = root.join(relative);
        ensure!(
            !absolute
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
            "Security scope escapes repository: {relative}"
        );
        let canonical = fs::canonicalize(&absolute)
            .with_context(|| format!("resolving security scope {relative}"))?;
        ensure!(
            path_within(&canonical, root),
            "Security scope resolves outside repository: {relative}"
        );
    }
    Ok(())
}

/// `dial-security-tree/v1` digest over the in-scope files' names, modes and contents plus HEAD.
pub(super) fn digest_working_tree(
    root: &Path,
    include: &[String],
    exclude: &[String],
) -> Result<String> {
    let mut files: Vec<String> = list_files(root)?
        .iter()
        .map(|f| normalize_relative(f))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|f| path_in_scope(f, include, exclude))
        .collect();
    files.sort();
    files.dedup();
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for relative in &files {
        let absolute = root.join(relative);
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        match fs::symlink_metadata(&absolute) {
            Err(_) => hasher.update(b"missing\0"),
            Ok(meta) => {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111
                };
                #[cfg(not(unix))]
                let exec = 0u32;
                hasher.update(format!("mode:{exec}\0").as_bytes());
                if meta.file_type().is_symlink() {
                    hasher.update(b"symlink\0");
                    let target = fs::read_link(&absolute)?;
                    hasher.update(target.to_string_lossy().as_bytes());
                } else if meta.is_file() {
                    let mut file = fs::File::open(&absolute)?;
                    let mut buffer = vec![0u8; 64 * 1024];
                    loop {
                        let read = std::io::Read::read(&mut file, &mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                } else {
                    hasher.update(b"unsupported\0");
                }
            }
        }
        hasher.update(b"\0");
    }
    let head = head_sha(root)?.unwrap_or_else(|| "unborn".to_owned());
    hasher.update(head.as_bytes());
    Ok(format!(
        "dial-security-tree/v1:sha256:{}",
        contracts::sha256_hex(&hasher.finalize())
    ))
}

fn normalize_target(root: &Path, request: &TargetRequest) -> Result<TargetPlan> {
    let display_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (kind, include, exclude) = match request {
        TargetRequest::Repository { include, exclude } => {
            (TargetKind::Repository, include, exclude)
        }
        TargetRequest::WorkingTree { include, exclude } => {
            (TargetKind::WorkingTree, include, exclude)
        }
        TargetRequest::ScopedPath { include, exclude } => {
            ensure!(
                include.iter().any(|v| !v.trim().is_empty()),
                "scoped_path security scans require at least one include path"
            );
            (TargetKind::ScopedPath, include, exclude)
        }
        TargetRequest::RefDiff {
            base,
            head,
            include,
            exclude,
        } => {
            let include = normalize_scope(include)?;
            let exclude = normalize_scope(exclude)?;
            validate_scope_paths(root, &include)?;
            validate_scope_paths(root, &exclude)?;
            let base_rev = resolve_ref(root, base)?
                .with_context(|| format!("Unknown security scan base revision: {base}"))?;
            let head_rev = resolve_ref(root, head)?
                .with_context(|| format!("Unknown security scan head revision: {head}"))?;
            let raw_diff =
                git_required(root, &["diff-tree", "-r", "--root", &base_rev, &head_rev])?;
            let material = json!({
                "baseRevision": base_rev,
                "headRevision": head_rev,
                "includePaths": include,
                "excludePaths": exclude,
                "rawDiff": raw_diff,
            });
            return Ok(TargetPlan {
                kind: TargetKind::RefDiff,
                repository_root: root.display().to_string(),
                display_name,
                revision: None,
                base_revision: Some(base_rev),
                head_revision: Some(head_rev),
                include_paths: include,
                exclude_paths: exclude,
                tree_digest: format!(
                    "dial-security-diff/v1:sha256:{}",
                    contracts::sha256_hex(contracts::canonical_json(&material).as_bytes())
                ),
            });
        }
    };
    let include = normalize_scope(include)?;
    let exclude = normalize_scope(exclude)?;
    validate_scope_paths(root, &include)?;
    validate_scope_paths(root, &exclude)?;
    Ok(TargetPlan {
        kind,
        repository_root: root.display().to_string(),
        display_name,
        revision: head_sha(root)?,
        base_revision: None,
        head_revision: None,
        include_paths: include.clone(),
        exclude_paths: exclude.clone(),
        tree_digest: digest_working_tree(root, &include, &exclude)?,
    })
}

fn normalize_knowledge_bases(paths: &[String], base: &Path) -> Result<Vec<KnowledgeBaseRef>> {
    let mut out = Vec::new();
    for input in paths {
        let canonical = fs::canonicalize(base.join(input))
            .with_context(|| format!("resolving security knowledge base {input}"))?;
        let meta = fs::metadata(&canonical)?;
        ensure!(
            meta.is_file(),
            "Security knowledge base is not a file: {input}"
        );
        let bytes = fs::read(&canonical)?;
        out.push(KnowledgeBaseRef {
            path: canonical.display().to_string(),
            sha256: contracts::sha256_hex(&bytes),
            size: bytes.len() as u64,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Validates the output directory and creates it if absent (mode 0700). A non-empty directory is
/// accepted only with `archive_existing`.
fn normalize_output(root: &Path, requested: &Path, archive_existing: bool) -> Result<OutputPlan> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()?.join(requested)
    };
    let parent = absolute
        .parent()
        .context("security output directory has no parent")?;
    let parent_real = fs::canonicalize(parent)
        .with_context(|| format!("resolving output parent {}", parent.display()))?;
    let name = absolute
        .file_name()
        .context("security output directory has no name")?;
    let candidate = parent_real.join(name);
    ensure!(
        !path_within(&candidate, root),
        "Security output directory must be outside the scanned repository"
    );
    let state = match fs::symlink_metadata(&candidate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&candidate)
                .with_context(|| format!("creating {}", candidate.display()))?;
            "empty"
        }
        Err(error) => return Err(error.into()),
        Ok(meta) => {
            ensure!(
                !meta.file_type().is_symlink(),
                "Security output directory must not be a symbolic link"
            );
            ensure!(
                meta.is_dir(),
                "Security output path exists and is not a directory"
            );
            ensure!(
                fs::canonicalize(&candidate)? == candidate,
                "Security output directory does not have a canonical identity"
            );
            if fs::read_dir(&candidate)?.next().is_none() {
                "empty"
            } else {
                ensure!(
                    archive_existing,
                    "Security output directory is not empty; enable archiveExisting or choose another directory"
                );
                "archivable"
            }
        }
    };
    set_private_dir_mode(&candidate)?;
    Ok(OutputPlan {
        root: candidate.display().to_string(),
        archive_existing,
        existing_state: state.to_owned(),
    })
}

fn set_private_dir_mode(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Builds the fingerprinted plan. Fails when `cwd` is not inside a Git repository.
pub fn create_plan(request: &PlanRequest<'_>, created_at: &str) -> Result<Plan> {
    let root_text = repository_root(request.cwd)?.with_context(|| {
        format!(
            "Security scans require a Git repository: {}",
            request.cwd.display()
        )
    })?;
    let root = fs::canonicalize(&root_text)?;
    let target = normalize_target(&root, &request.target)?;
    let knowledge_bases = normalize_knowledge_bases(&request.knowledge_bases, &root)?;
    let output = normalize_output(&root, request.output_root, request.archive_existing)?;
    let config_fingerprint = format!(
        "dial-security-config/v1:sha256:{}",
        contracts::sha256_hex(contracts::canonical_json(&request.config_material).as_bytes())
    );
    let repository_root = root.display().to_string();
    let material = json!({
        "repositoryRoot": repository_root,
        "target": target,
        "knowledgeBases": knowledge_bases,
        "output": output,
        "model": request.model,
        "account": request.account,
        "configFingerprint": config_fingerprint,
        "workflowFingerprint": request.workflow_fingerprint,
    });
    let fingerprint = format!(
        "dial-security-plan/v1:sha256:{}",
        contracts::sha256_hex(contracts::canonical_json(&material).as_bytes())
    );
    Ok(Plan {
        document_type: contracts::PLAN_DOCUMENT_TYPE.to_owned(),
        schema_version: contracts::SCHEMA_VERSION.to_owned(),
        id: contracts::plan_id(&fingerprint),
        created_at: created_at.to_owned(),
        repository_root,
        target,
        knowledge_bases,
        output,
        model: request.model.clone(),
        account: request.account.clone(),
        config_fingerprint,
        workflow_fingerprint: request.workflow_fingerprint.clone(),
        fingerprint,
    })
}

/// Recomputes the plan from its own inputs and checks the fingerprint still matches.
/// Errors with a stale-plan message otherwise.
pub fn assert_fresh(plan: &Plan, config_material: Value) -> Result<()> {
    let target_request = match plan.target.kind {
        TargetKind::RefDiff => TargetRequest::RefDiff {
            base: plan.target.base_revision.clone().unwrap_or_default(),
            head: plan.target.head_revision.clone().unwrap_or_default(),
            include: plan.target.include_paths.clone(),
            exclude: plan.target.exclude_paths.clone(),
        },
        TargetKind::ScopedPath => TargetRequest::ScopedPath {
            include: plan.target.include_paths.clone(),
            exclude: plan.target.exclude_paths.clone(),
        },
        TargetKind::WorkingTree => TargetRequest::WorkingTree {
            include: plan.target.include_paths.clone(),
            exclude: plan.target.exclude_paths.clone(),
        },
        TargetKind::Repository | TargetKind::Imported => TargetRequest::Repository {
            include: plan.target.include_paths.clone(),
            exclude: plan.target.exclude_paths.clone(),
        },
    };
    let knowledge: Vec<String> = plan
        .knowledge_bases
        .iter()
        .map(|k| k.path.clone())
        .collect();
    let output = PathBuf::from(&plan.output.root);
    let cwd = PathBuf::from(&plan.repository_root);
    let current = create_plan(
        &PlanRequest {
            cwd: &cwd,
            target: target_request,
            knowledge_bases: knowledge,
            output_root: &output,
            archive_existing: plan.output.archive_existing,
            model: plan.model.clone(),
            account: plan.account.clone(),
            config_material,
            workflow_fingerprint: plan.workflow_fingerprint.clone(),
        },
        &plan.created_at,
    );
    // The output directory already exists on the second pass; a non-empty archivable directory
    // is accepted only when the plan recorded that it may be archived.
    let current = current?;
    if current.fingerprint != plan.fingerprint {
        bail!(
            "Security scan plan is stale: expected {}, got {}. Run security preflight again.",
            plan.fingerprint,
            current.fingerprint
        );
    }
    Ok(())
}
