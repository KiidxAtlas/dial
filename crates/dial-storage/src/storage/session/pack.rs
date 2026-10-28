//! Portable session archives (`dial session pack` / `unpack`, `/pack`).
//!
//! An archive is a zip holding a `manifest.json` plus the session journal verbatim, its side-thread
//! and stream-journal companions, child transcripts, and every artifact blob the journal references,
//! each named by its SHA-256. The member grammar is fixed: nothing outside it is admitted. Unpacking
//! stages and verifies every member against the manifest before anything is installed, refuses to
//! overwrite any existing file, and rolls back what it created if an install step fails.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::migrate::{CURRENT_SCHEMA_VERSION, header_version};
use super::{Session, artifact_root, read_bounded_line, transfers, valid_id};
use crate::config::paths::Layout;
use crate::platform::fs as platform_fs;
use crate::storage::artifact::ArtifactStore;
use crate::storage::artifact::transfer::{collect_references, file_sha256};
use crate::storage::{side_thread, subagent};

/// Archive format name recorded in the manifest.
pub const FORMAT: &str = "dial-session-pack";
/// Archive format version this Dial writes and reads.
pub const PACK_VERSION: u32 = 1;

const MANIFEST: &str = "manifest.json";
const SESSION_MEMBER: &str = "session.jsonl";
const SIDE_MEMBER: &str = "side.json";
const STREAM_MEMBER: &str = "stream.journal";
const SUBAGENT_PREFIX: &str = "subagents/";
const ARTIFACT_PREFIX: &str = "artifacts/";

/// Size ceilings. Pack and unpack both enforce them; the defaults bound a hostile archive's disk use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest single member, in bytes.
    pub member_bytes: u64,
    /// Largest sum of member bytes.
    pub total_bytes: u64,
    /// Largest member count, including the manifest.
    pub members: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            member_bytes: 512 << 20,
            total_bytes: 2 << 30,
            members: 4096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberEntry {
    name: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: u32,
    schema_version: u32,
    session_id: String,
    session_file: String,
    /// Path the session lived at when packed. Used only to fork a stale copy left there.
    source_path: PathBuf,
    members: Vec<MemberEntry>,
    /// `artifact://<sha256>` references whose blobs are members under `artifacts/`.
    artifacts: Vec<String>,
}

/// What `pack_session` wrote.
#[derive(Debug)]
pub struct PackReport {
    pub session_id: String,
    pub members: usize,
    pub artifacts: usize,
    pub subagents: usize,
    pub bytes: u64,
}

/// What `unpack_session` installed.
#[derive(Debug)]
pub struct UnpackReport {
    pub session_id: String,
    pub session_path: PathBuf,
    pub artifacts: usize,
    pub subagents: usize,
}

struct Source {
    name: String,
    path: PathBuf,
}

struct Staged {
    path: PathBuf,
    sha256: String,
    bytes: u64,
}

/// Packs the session at `session_path` with its companions into a new archive at `out`.
pub fn pack_session(layout: &Layout, session_path: &Path, out: &Path) -> Result<PackReport> {
    pack_session_with(layout, session_path, out, Limits::default())
}

/// Unpacks `archive` into this project's session directory. Nothing is overwritten.
pub fn unpack_session(layout: &Layout, archive: &Path) -> Result<UnpackReport> {
    unpack_session_with(layout, archive, Limits::default())
}

fn pack_session_with(
    layout: &Layout,
    session_path: &Path,
    out: &Path,
    limits: Limits,
) -> Result<PackReport> {
    ensure!(
        is_regular(session_path)?,
        "session {} is not a regular file",
        session_path.display()
    );
    let snapshot = Session::read(session_path)?;
    let session_id = snapshot.id.clone();
    ensure!(
        valid_id(&session_id),
        "session id is not a valid archive id"
    );
    let source_root = artifact_root(session_path)?;

    let mut sources = vec![Source {
        name: SESSION_MEMBER.to_owned(),
        path: session_path.to_path_buf(),
    }];
    let side = side_thread::path_for(session_path);
    if is_regular(&side)? {
        sources.push(Source {
            name: SIDE_MEMBER.to_owned(),
            path: side,
        });
    }

    // Artifacts referenced by the journal, or by live stream nodes, travel with the session.
    let mut refs = HashSet::new();
    let mut reader = BufReader::new(File::open(session_path)?);
    let mut line = Vec::new();
    while let Some(raw) = read_bounded_line(&mut reader, &mut line)? {
        if raw.terminated
            && !raw.oversized
            && let Ok(value) = serde_json::from_slice::<Value>(&line)
        {
            collect_references(&value, &source_root, &mut refs);
        }
    }
    let stream = crate::runtime::stream_journal::path(session_path);
    for node in crate::runtime::stream_journal::recover(session_path)? {
        collect_references(&serde_json::to_value(node)?, &source_root, &mut refs);
    }
    if is_regular(&stream)? {
        sources.push(Source {
            name: STREAM_MEMBER.to_owned(),
            path: stream,
        });
    }

    let transcripts = subagent::transcript_files(layout, &session_id)?;
    for path in &transcripts {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("child transcript has no UTF-8 file name")?;
        ensure!(
            safe_jsonl_name(file),
            "child transcript {file:?} has an unsupported name"
        );
        sources.push(Source {
            name: format!("{SUBAGENT_PREFIX}{file}"),
            path: path.clone(),
        });
    }

    let mut uris: Vec<String> = refs.into_iter().collect();
    uris.sort();
    if !uris.is_empty() {
        let store = ArtifactStore::new(&source_root)?;
        for uri in &uris {
            let id = crate::storage::artifact::artifact_id(uri).map_err(anyhow::Error::msg)?;
            let blob = store.blob_path(uri)?;
            ensure!(
                is_regular(&blob)?,
                "referenced artifact {uri} is missing from {}",
                source_root.display()
            );
            ensure!(
                file_sha256(&blob)? == id,
                "referenced artifact {uri} has corrupt content"
            );
            sources.push(Source {
                name: format!("{ARTIFACT_PREFIX}{id}"),
                path: blob,
            });
        }
    }

    ensure!(
        sources.len() < limits.members,
        "session archive would hold {} members, over the limit of {}",
        sources.len() + 1,
        limits.members
    );
    let mut total = 0u64;
    for source in &sources {
        let len = fs::symlink_metadata(&source.path)?.len();
        ensure!(
            len <= limits.member_bytes,
            "{} is {len} bytes, over the {} byte member limit",
            source.name,
            limits.member_bytes
        );
        total += len;
    }
    ensure!(
        total <= limits.total_bytes,
        "session archive would hold {total} bytes, over the {} byte limit",
        limits.total_bytes
    );

    let manifest = Manifest {
        format: FORMAT.to_owned(),
        version: PACK_VERSION,
        schema_version: CURRENT_SCHEMA_VERSION,
        session_id: session_id.clone(),
        session_file: session_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("session path has no UTF-8 file name")?
            .to_owned(),
        source_path: session_path.to_path_buf(),
        members: Vec::new(),
        artifacts: uris.clone(),
    };
    let written = write_archive(out, &sources, manifest, limits.member_bytes)?;
    let bytes = written.members.iter().map(|m| m.bytes).sum();
    Ok(PackReport {
        session_id,
        members: written.members.len() + 1,
        artifacts: uris.len(),
        subagents: transcripts.len(),
        bytes,
    })
}

/// Writes the archive, streaming each member once through SHA-256. The manifest is written last
/// because it records what was hashed. Returns the completed manifest.
fn write_archive(
    out: &Path,
    sources: &[Source],
    mut manifest: Manifest,
    member_limit: u64,
) -> Result<Manifest> {
    // Opening is outside the cleanup path: a refused open means the archive was never ours to remove.
    let file = platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), out)
        .with_context(|| {
        format!(
            "creating {} (an existing archive is never overwritten)",
            out.display()
        )
    })?;
    match write_members(file, sources, &mut manifest, member_limit) {
        Ok(()) => Ok(manifest),
        Err(error) => {
            let _ = fs::remove_file(out);
            Err(error)
        }
    }
}

fn write_members(
    file: std::fs::File,
    sources: &[Source],
    manifest: &mut Manifest,
    member_limit: u64,
) -> Result<()> {
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for source in sources {
        zip.start_file(source.name.as_str(), options)?;
        let mut input =
            platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), &source.path)
                .with_context(|| format!("reading {}", source.path.display()))?;
        let (sha256, bytes) = copy_hashed(&mut input, &mut zip, member_limit)
            .with_context(|| format!("archiving {}", source.name))?;
        manifest.members.push(MemberEntry {
            name: source.name.clone(),
            sha256,
            bytes,
        });
    }
    zip.start_file(MANIFEST, options)?;
    zip.write_all(&serde_json::to_vec_pretty(manifest)?)?;
    let file = zip.finish()?;
    file.sync_all()?;
    Ok(())
}

fn unpack_session_with(layout: &Layout, archive: &Path, limits: Limits) -> Result<UnpackReport> {
    ensure!(
        is_regular(archive)?,
        "session archive {} is not a regular file",
        archive.display()
    );
    crate::platform::atomic::private_dir(&layout.state_dir)?;
    let staging = tempfile::Builder::new()
        .prefix("unpack-")
        .tempdir_in(&layout.state_dir)?;
    let mut staged = stage(archive, staging.path(), limits)?;

    let manifest_file = staged
        .remove(MANIFEST)
        .context("session archive has no manifest.json")?;
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(&manifest_file.path).context("reading archive manifest")?)
            .context("parsing archive manifest")?;
    verify_manifest(&manifest, &staged)?;

    let session_staged = &staged[SESSION_MEMBER];
    verify_journal(&manifest, &session_staged.path)?;

    let sessions_dir = layout.sessions_dir();
    crate::platform::atomic::private_dir(&sessions_dir)?;
    let dst = sessions_dir.join(&manifest.session_file);

    // Every destination is checked before the first write, so a conflict installs nothing.
    let mut plan: Vec<(PathBuf, PathBuf)> = vec![(session_staged.path.clone(), dst.clone())];
    if let Some(side) = staged.get(SIDE_MEMBER) {
        plan.push((side.path.clone(), side_thread::path_for(&dst)));
    }
    if let Some(stream) = staged.get(STREAM_MEMBER) {
        plan.push((
            stream.path.clone(),
            crate::runtime::stream_journal::path(&dst),
        ));
    }
    let transcript_dir = subagent::transcript_dir(layout, &manifest.session_id);
    let mut subagents = 0usize;
    for (name, file) in &staged {
        if let Some(leaf) = name.strip_prefix(SUBAGENT_PREFIX) {
            crate::platform::atomic::private_dir(&transcript_dir)?;
            plan.push((file.path.clone(), transcript_dir.join(leaf)));
            subagents += 1;
        }
    }
    for (_, target) in &plan {
        ensure!(
            !exists(target)?,
            "{} already exists; refusing to overwrite it",
            target.display()
        );
    }

    let artifact_ids: Vec<String> = manifest
        .artifacts
        .iter()
        .map(|uri| crate::storage::artifact::artifact_id(uri).map_err(anyhow::Error::msg))
        .collect::<Result<_>>()?;

    let mut created = Vec::new();
    let installed = (|| -> Result<()> {
        for (src, target) in &plan {
            install_file(src, target, &mut created)?;
        }
        if !artifact_ids.is_empty() {
            let store = ArtifactStore::new(&artifact_root(&dst)?)?;
            for id in &artifact_ids {
                let member = staged
                    .get(&format!("{ARTIFACT_PREFIX}{id}"))
                    .context("artifact member missing after verification")?;
                store.import_blob(&format!("artifact://{id}"), &member.path)?;
            }
        }
        transfers::record(layout, &manifest.session_id, &manifest.source_path, &dst)
            .context("recording the unpacked session in the transfer ledger")?;
        Ok(())
    })();
    if let Err(error) = installed {
        for path in created.iter().rev() {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }

    Ok(UnpackReport {
        session_id: manifest.session_id,
        session_path: dst,
        artifacts: artifact_ids.len(),
        subagents,
    })
}

/// Reads every member into `dir`, refusing anything outside the grammar and anything over limits.
fn stage(archive: &Path, dir: &Path, limits: Limits) -> Result<HashMap<String, Staged>> {
    let mut zip = zip::ZipArchive::new(File::open(archive)?)
        .context("reading session archive; it is not a valid zip file")?;
    ensure!(
        zip.len() <= limits.members,
        "session archive holds {} members, over the limit of {}",
        zip.len(),
        limits.members
    );
    let mut staged = HashMap::new();
    let mut total = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name()?.into_owned();
        ensure!(
            !entry.is_dir(),
            "archive member {name:?} is a directory; refusing it"
        );
        ensure!(
            entry
                .unix_mode()
                .is_none_or(|mode| mode & 0o170000 != 0o120000),
            "archive member {name:?} is a symlink; refusing it"
        );
        validate_member_name(&name)?;
        ensure!(
            !staged.contains_key(&name),
            "archive member {name:?} appears more than once"
        );
        let budget = limits.member_bytes.min(limits.total_bytes - total);
        ensure!(
            entry.size() <= budget,
            "archive member {name:?} exceeds the archive size limits"
        );
        let path = dir.join(index.to_string());
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let (sha256, bytes) = copy_hashed(&mut entry, &mut out, budget)
            .with_context(|| format!("extracting archive member {name:?}"))?;
        total += bytes;
        staged.insert(
            name,
            Staged {
                path,
                sha256,
                bytes,
            },
        );
    }
    Ok(staged)
}

/// Checks the manifest against the format, version, schema, and the staged bytes it describes.
fn verify_manifest(manifest: &Manifest, staged: &HashMap<String, Staged>) -> Result<()> {
    ensure!(
        manifest.format == FORMAT,
        "not a Dial session archive (format {:?})",
        manifest.format
    );
    ensure!(
        manifest.version <= PACK_VERSION,
        "archive format version {} is newer than the version {PACK_VERSION} this Dial understands; update Dial to open it",
        manifest.version
    );
    ensure!(
        manifest.version == PACK_VERSION,
        "unsupported archive format version {}",
        manifest.version
    );
    ensure!(
        manifest.schema_version <= CURRENT_SCHEMA_VERSION,
        "session journal uses schema version {}, newer than the version {CURRENT_SCHEMA_VERSION} this Dial understands; update Dial to open it",
        manifest.schema_version
    );
    ensure!(
        valid_id(&manifest.session_id),
        "archive session id is not a valid identifier"
    );
    ensure!(
        safe_jsonl_name(&manifest.session_file),
        "archive session file name {:?} is not allowed",
        manifest.session_file
    );

    let mut declared = BTreeSet::new();
    for member in &manifest.members {
        validate_member_name(&member.name)?;
        ensure!(
            declared.insert(member.name.clone()),
            "archive manifest lists member {:?} twice",
            member.name
        );
        let file = staged
            .get(&member.name)
            .with_context(|| format!("archive is missing member {:?}", member.name))?;
        ensure!(
            file.sha256 == member.sha256 && file.bytes == member.bytes,
            "archive member {:?} does not match its manifest entry; the archive is corrupt",
            member.name
        );
    }
    ensure!(
        declared.contains(SESSION_MEMBER),
        "session archive has no session journal"
    );
    ensure!(
        staged.len() == declared.len(),
        "session archive contains members its manifest does not list"
    );

    let mut listed = BTreeSet::new();
    for uri in &manifest.artifacts {
        let id = crate::storage::artifact::artifact_id(uri).map_err(anyhow::Error::msg)?;
        ensure!(listed.insert(id), "archive lists artifact {uri} twice");
    }
    let members: BTreeSet<String> = declared
        .iter()
        .filter_map(|name| name.strip_prefix(ARTIFACT_PREFIX).map(str::to_owned))
        .collect();
    ensure!(
        listed == members,
        "archive artifact list does not match its artifact members"
    );
    Ok(())
}

/// The staged journal must be a valid session whose header names the manifest's session.
fn verify_journal(manifest: &Manifest, journal: &Path) -> Result<()> {
    let mut first = String::new();
    BufReader::new(File::open(journal)?).read_line(&mut first)?;
    let header: Value = serde_json::from_str(first.trim_end()).context("parsing session header")?;
    ensure!(
        header.get("id").and_then(Value::as_str) == Some(manifest.session_id.as_str()),
        "session journal header names a different session than the archive manifest"
    );
    header_version(&header)?;
    let snapshot = Session::read(journal)?;
    ensure!(
        snapshot.id == manifest.session_id,
        "session journal does not match the archive manifest"
    );
    Ok(())
}

/// Admits only the fixed member grammar. Traversal, absolute paths, and backslashes never match.
fn validate_member_name(name: &str) -> Result<()> {
    ensure!(
        !name.contains('\\')
            && !name.starts_with('/')
            && !name
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == ".."),
        "unsafe archive member path {name:?}"
    );
    match name {
        SESSION_MEMBER | SIDE_MEMBER | STREAM_MEMBER | MANIFEST => Ok(()),
        _ => {
            if let Some(leaf) = name.strip_prefix(SUBAGENT_PREFIX) {
                ensure!(
                    safe_jsonl_name(leaf),
                    "unsupported child transcript name {leaf:?}"
                );
            } else if let Some(id) = name.strip_prefix(ARTIFACT_PREFIX) {
                ensure!(
                    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                    "unsupported artifact member name {id:?}"
                );
            } else {
                bail!("unexpected archive member {name:?}");
            }
            Ok(())
        }
    }
}

/// A plain single-component `*.jsonl` name from `[A-Za-z0-9_.-]`, never hidden.
fn safe_jsonl_name(name: &str) -> bool {
    !name.starts_with('.')
        && name.len() > ".jsonl".len()
        && name.ends_with(".jsonl")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Copies `input` to `out` while hashing it. Fails once more than `limit` bytes flow through.
fn copy_hashed(input: &mut impl Read, out: &mut impl Write, limit: u64) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= limit, "content exceeds the {limit} byte limit");
        hash.update(&buffer[..n]);
        out.write_all(&buffer[..n])?;
    }
    Ok((format!("{:x}", hash.finalize()), total))
}

/// Copies a staged file into `dst` without ever replacing an existing file, then records it so a
/// later failure can remove it.
fn install_file(src: &Path, dst: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let dir = dst
        .parent()
        .context("install destination has no directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    platform_fs::set_owner_only(tmp.path())?;
    io::copy(&mut File::open(src)?, tmp.as_file_mut())?;
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(dst) {
        Ok(_) => created.push(dst.to_path_buf()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            bail!("{} already exists; refusing to overwrite it", dst.display())
        }
        Err(error) => return Err(error.error.into()),
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Whether `path` names a regular file. A missing path is `false`; a non-regular one is an error.
fn is_regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => bail!("{} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

/// Whether anything (including a dangling symlink) occupies `path`.
fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("checking {}", path.display())),
    }
}

#[cfg(test)]
mod tests;
