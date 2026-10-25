//! Git fetch for skill packages, plus the safe copy and content manifest used to store them.

use crate::storage::skills::{
    HEAD_BYTES, SKILL_FILE, Scope, field, load_named, parse_fields, read_head, split_frontmatter,
    valid_name,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::source::Source;

/// Upper bound for one git command; a stalled host must not hold the install forever.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_SEARCH_DEPTH: usize = 6;
const MAX_TREE_DEPTH: usize = 12;
pub(super) const MAX_PACKAGE_SKILLS: usize = 256;
pub(super) const MAX_PACKAGE_FILES: usize = 2000;
pub(super) const MAX_PACKAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// A fetched repository. The directory is deleted when this value drops.
pub(super) struct Checkout {
    pub(super) dir: tempfile::TempDir,
    pub(super) commit: String,
}

/// Fetches `source` at its ref (or the remote HEAD) into a temporary checkout. Local paths resolve against `base`.
pub(super) fn fetch(
    source: &Source,
    base: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<Checkout> {
    source.validate()?;
    let url = resolve_local(&source.url, base);
    progress(&format!("Fetching {}", source.spec()));
    let dir = tempfile::Builder::new()
        .prefix("dial-skills-")
        .tempdir()
        .context("creating a temporary checkout")?;
    let root = dir.path();
    // An empty template keeps a user's init templates (which may carry hooks) out of the checkout.
    run_git(&["init", "--quiet", "--template="], root, cancel)?;
    let rev = source.reference.as_deref().unwrap_or("HEAD");
    run_git(
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "--no-tags",
            "--",
            &url,
            rev,
        ],
        root,
        cancel,
    )?;
    run_git(
        &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
        root,
        cancel,
    )?;
    let commit = run_git(&["rev-parse", "HEAD"], root, cancel)?
        .trim()
        .to_owned();
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "git returned an unexpected commit id {commit:?}"
    );
    Ok(Checkout { dir, commit })
}

/// Makes a local repository path absolute so git, which runs inside a temp directory, finds it.
pub(super) fn resolve_local(url: &str, base: &Path) -> String {
    // Schemes and scp-style remotes keep their identity even if a same-named local path exists.
    if url.contains(':') && !Path::new(url).is_absolute() {
        return url.to_owned();
    }
    let candidate = Path::new(url);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    match fs::canonicalize(&joined) {
        Ok(path) if !url.contains("://") => path.to_string_lossy().into_owned(),
        _ => url.to_owned(),
    }
}

fn run_git(args: &[&str], cwd: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a timeout or cancel also stops transport helpers
    // (`git-remote-https`, `ssh`) that would otherwise keep the output pipes open.
    let mut child = crate::platform::process::spawn_group(&mut command).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!("git is required to install skill packages; install Git and try again")
        } else {
            anyhow!("starting git: {error}")
        }
    })?;
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for git")? {
            break status;
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = crate::platform::process::kill_group(child.id());
            let _ = child.wait();
            bail!("cancelled while running git {}", args[0]);
        }
        if started.elapsed() > GIT_TIMEOUT {
            let _ = crate::platform::process::kill_group(child.id());
            let _ = child.wait();
            bail!(
                "git {} timed out after {} seconds",
                args[0],
                GIT_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(40));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    if !status.success() {
        let message = String::from_utf8_lossy(&err);
        let detail = message
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no details");
        bail!("git {} failed: {}", args[0], detail.trim());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Reads a child pipe on its own thread, bounded so a chatty command cannot exhaust memory.
fn spawn_reader<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let _ = pipe.take(1024 * 1024).read_to_end(&mut bytes);
        }
        bytes
    })
}

#[derive(Default)]
struct Budget {
    files: usize,
    bytes: u64,
}

/// Copies every skill under `path` (or the whole checkout) into `dest`, which must be empty. Returns the sorted
/// skill names. Symbolic links and special files inside a skill are refused; links elsewhere are ignored.
pub(super) fn copy_skills(checkout: &Path, path: Option<&str>, dest: &Path) -> Result<Vec<String>> {
    let base = match path {
        Some(path) => {
            let mut current = checkout.to_path_buf();
            for component in Path::new(path).components() {
                if let Component::Normal(name) = component {
                    current.push(name);
                    let meta = fs::symlink_metadata(&current)
                        .with_context(|| format!("path {path:?} is not in the repository"))?;
                    ensure!(
                        !meta.file_type().is_symlink(),
                        "path {path:?} goes through a symbolic link, which is refused"
                    );
                }
            }
            ensure!(
                current.is_dir(),
                "path {path:?} is not a directory in the repository"
            );
            current
        }
        None => checkout.to_path_buf(),
    };
    let mut dirs = Vec::new();
    if is_skill_dir(&base) {
        dirs.push(base.clone());
    } else {
        find_skill_dirs(&base, 0, &mut dirs)?;
    }
    ensure!(
        !dirs.is_empty(),
        "no skills found: expected directories containing {SKILL_FILE}{}",
        path.map(|p| format!(" under {p:?}")).unwrap_or_default()
    );
    ensure!(
        dirs.len() <= MAX_PACKAGE_SKILLS,
        "package has more than {MAX_PACKAGE_SKILLS} skills"
    );
    let mut names: Vec<String> = Vec::new();
    let mut budget = Budget::default();
    for dir in &dirs {
        let name = declared_name(dir)?;
        ensure!(
            !names.contains(&name),
            "two skills in the package are named {name:?}"
        );
        let target = dest.join(&name);
        copy_tree(dir, &target, 0, &mut budget, &name)?;
        ensure!(
            load_named(&target, &name, Scope::User).is_some(),
            "skill {name:?} is not valid: its {SKILL_FILE} needs a non-empty description"
        );
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn is_skill_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(SKILL_FILE)).is_ok_and(|meta| meta.is_file())
}

fn find_skill_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata` keeps links from being followed out of the checkout.
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        if is_skill_dir(&path) {
            out.push(path);
            if out.len() > MAX_PACKAGE_SKILLS {
                return Ok(());
            }
        } else if depth + 1 < MAX_SEARCH_DEPTH {
            find_skill_dirs(&path, depth + 1, out)?;
        }
    }
    Ok(())
}

/// The skill's name: its frontmatter `name`, else its directory name. Must be a valid skill directory name.
fn declared_name(dir: &Path) -> Result<String> {
    let (bytes, _) = read_head(&dir.join(SKILL_FILE), HEAD_BYTES)?;
    let text = String::from_utf8_lossy(&bytes);
    let fallback = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = split_frontmatter(&text)
        .and_then(|(frontmatter, _)| {
            field(&parse_fields(frontmatter), "name").map(|f| f.value.trim().to_owned())
        })
        .unwrap_or(fallback);
    ensure!(
        valid_name(&name),
        "skill name {name:?} in {} is not a valid name (use lowercase letters, digits and '-', up to 64 characters)",
        dir.display()
    );
    Ok(name)
}

fn copy_tree(src: &Path, dst: &Path, depth: usize, budget: &mut Budget, skill: &str) -> Result<()> {
    ensure!(
        depth <= MAX_TREE_DEPTH,
        "skill {skill:?} nests deeper than {MAX_TREE_DEPTH} levels"
    );
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut entries: Vec<_> = fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            bail!("skill {skill:?} has a file name that is not UTF-8");
        };
        ensure!(
            !name.contains(['\\', '\0', '/']),
            "skill {skill:?} has an unsafe file name {name:?}"
        );
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let meta = fs::symlink_metadata(&from)?;
        let file_type = meta.file_type();
        ensure!(
            !file_type.is_symlink(),
            "symbolic link refused in skill {skill:?}: {name}"
        );
        if file_type.is_dir() {
            copy_tree(&from, &to, depth + 1, budget, skill)?;
        } else if file_type.is_file() {
            budget.files += 1;
            ensure!(
                budget.files <= MAX_PACKAGE_FILES,
                "package has more than {MAX_PACKAGE_FILES} files"
            );
            ensure!(
                meta.len() <= MAX_FILE_BYTES,
                "file {name:?} in skill {skill:?} exceeds {} MiB",
                MAX_FILE_BYTES / (1024 * 1024)
            );
            budget.bytes += meta.len();
            ensure!(
                budget.bytes <= MAX_PACKAGE_BYTES,
                "package exceeds {} MiB",
                MAX_PACKAGE_BYTES / (1024 * 1024)
            );
            let bytes = fs::read(&from).with_context(|| format!("reading {name}"))?;
            write_new(&to, &bytes, is_executable(&meta))?;
        } else {
            bail!("unsupported file type in skill {skill:?}: {name}");
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o755 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Relative file path → `x`/`-` exec flag plus SHA-256 of contents. Sorted, so equal trees compare equal.
pub(super) fn manifest(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    walk_manifest(root, root, &mut out)?;
    Ok(out)
}

fn walk_manifest(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_dir() {
            walk_manifest(root, &path, out)?;
        } else if meta.file_type().is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|rel| {
                    rel.components()
                        .filter_map(|c| c.as_os_str().to_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .with_context(|| format!("unexpected path {}", path.display()))?;
            let bytes = fs::read(&path).with_context(|| format!("reading {rel}"))?;
            let digest = crate::hex(&Sha256::digest(&bytes));
            let flag = if is_executable(&meta) { "x" } else { "-" };
            out.insert(rel, format!("{flag}{digest}"));
        } else {
            bail!(
                "unsupported file type in stored package: {}",
                path.display()
            );
        }
    }
    Ok(())
}

/// Stable identifier of a stored tree: SHA-256 over its manifest. Also the store directory name.
pub(super) fn integrity(manifest: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (rel, value) in manifest {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    crate::hex(&hasher.finalize())
}

#[cfg(test)]
mod tests;
