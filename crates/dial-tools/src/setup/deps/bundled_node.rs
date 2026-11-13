//! Node.js runtime shipped inside release builds (see `build.rs` and `DIAL_BUNDLED_NODE_DIR`).
//!
//! The payload is extracted lazily into Dial's user cache, never downloaded. Extraction streams
//! through a bounded gzip decoder into an owner-private temporary file, checks length and SHA-256,
//! then publishes it with a no-clobber rename. Every later use re-checks the same digest, so a
//! corrupted or replaced runtime fails closed instead of falling back to `PATH`.

include!(concat!(env!("OUT_DIR"), "/bundled_node.rs"));

use crate::platform::{atomic, fs as platform_fs};
use crate::tools::exec::kernel;
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// One embedded runtime: what the build recorded and the compressed bytes that produce it.
#[derive(Clone, Copy)]
struct Bundle<'a> {
    version: &'a str,
    sha256: &'a str,
    size: u64,
    gz: &'a [u8],
}

/// Whether this build embeds a Node.js runtime.
pub fn available() -> bool {
    embedded().is_some()
}

/// The embedded Node.js version, such as `24.22.3`.
pub fn version() -> Option<&'static str> {
    VERSION
}

/// Path to the verified bundled `node` executable, extracting it on first use. `Ok(None)` means
/// this build carries no runtime. An error means the runtime is present but unusable, and callers
/// must not substitute another program.
pub fn provision() -> Result<Option<PathBuf>, String> {
    let Some(bundle) = embedded() else {
        return Ok(None);
    };
    let user_dir = crate::config::paths::user_dir()
        .map_err(|e| format!("cannot access the bundled Node.js runtime: {e:#}"))?;
    provision_in(&user_dir, &bundle)
        .map(Some)
        .map_err(|e| format!("bundled Node.js runtime: {e:#}"))
}

/// Resolve a Dial-owned program for an operator-facing label, such as `node` for `eval.node`.
/// Identical to the crate-internal resolver: the bundled runtime is preferred for the default
/// `node` only, and the outside-workspace identity check still applies.
pub fn resolve(workspace: &Path, program: &str, label: &str) -> Result<PathBuf, String> {
    kernel::resolve_operator_executable(workspace, program, label)
}

fn embedded() -> Option<Bundle<'static>> {
    let version = VERSION?;
    (!PAYLOAD.is_empty()).then_some(Bundle {
        version,
        sha256: SHA256,
        size: SIZE,
        gz: PAYLOAD,
    })
}

fn provision_in(user_dir: &Path, bundle: &Bundle<'_>) -> Result<PathBuf> {
    ensure!(
        bundle.sha256.len() == 64 && bundle.size > 0,
        "embedded metadata is malformed"
    );
    let root = user_dir.join("cache").join("runtimes").join("node");
    atomic::private_dir(&root)?;
    let dir = root.join(format!("node-{}-{}", bundle.version, &bundle.sha256[..16]));
    atomic::private_dir(&dir)?;
    let target = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
    match fs::symlink_metadata(&target) {
        Ok(_) => verify(&target, bundle)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => extract(&dir, &target, bundle)?,
        Err(error) => return Err(error.into()),
    }
    Ok(target)
}

/// Re-check an existing runtime: a regular, owner-only, unlinked file whose bytes still match.
fn verify(target: &Path, bundle: &Bundle<'_>) -> Result<()> {
    let meta = fs::symlink_metadata(target)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "{} is not a regular file; remove it and let Dial extract the runtime again",
        target.display()
    );
    let file = platform_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), target)?;
    atomic::check_file(&file)?;
    let (total, digest) = hash_stream(&file, &mut io::sink(), bundle.size + 1)?;
    if total != bundle.size || digest != bundle.sha256 {
        bail!(
            "{} does not match the embedded runtime digest; remove it to re-extract, or reinstall Dial",
            target.display()
        );
    }
    Ok(())
}

/// Decompress into a staged file, verify it completely, then publish without replacing anything.
fn extract(dir: &Path, target: &Path, bundle: &Bundle<'_>) -> Result<()> {
    let mut staged = tempfile::Builder::new()
        .prefix(".node-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .context("staging the bundled Node.js runtime")?;
    let (total, digest) = hash_stream(
        GzDecoder::new(bundle.gz),
        staged.as_file_mut(),
        bundle.size + 1,
    )
    .context("decompressing the bundled Node.js runtime")?;
    ensure!(
        total == bundle.size && digest == bundle.sha256,
        "embedded Node.js payload does not match its build metadata"
    );
    staged.as_file_mut().flush()?;
    platform_fs::sync_file(staged.as_file())?;
    #[cfg(unix)]
    platform_fs::set_mode(staged.path(), 0o700)?;
    #[cfg(windows)]
    platform_fs::set_owner_only_file(staged.as_file())?;
    match staged.persist_noclobber(target) {
        Ok(_) => Ok(()),
        // A concurrent Dial published first. Its file must pass the same checks before use.
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => verify(target, bundle),
        Err(error) => Err(error.error.into()),
    }
}

/// Copy at most `limit` bytes while hashing them. Callers pass `size + 1` so overflow is seen
/// as a length mismatch instead of being silently truncated.
fn hash_stream(reader: impl Read, sink: &mut impl Write, limit: u64) -> io::Result<(u64, String)> {
    let mut limited = reader.take(limit);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match limited.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buf[..read]);
        sink.write_all(&buf[..read])?;
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

#[cfg(test)]
mod tests;
