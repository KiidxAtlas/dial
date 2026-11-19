//! Relocatable CPython from a release-prepared, build-verified archive. Never run a downloader,
//! reuse mutable stdlib files, or search the working directory. The launch owner holds `Runtime`
//! until the interpreter exits (imports can open stdlib files long after the first cell).

use anyhow::{Context, Result, ensure};
use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
};

mod payload {
    include!(concat!(env!("OUT_DIR"), "/bundled_python.rs"));
}

const PROGRAM: &str = if cfg!(windows) {
    "python.exe"
} else {
    "bin/python3"
};

pub fn available() -> bool {
    payload::VERSION.is_some()
}
pub fn version() -> Option<&'static str> {
    payload::VERSION
}

#[derive(Debug)]
pub struct Runtime {
    tree: tempfile::TempDir,
    executable: PathBuf,
}

impl Runtime {
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    pub(crate) fn root(&self) -> &Path {
        self.tree.path()
    }
}

/// Installed-artifact probe (not a PATH fallback). Normal execution validates the parent against
/// the workspace policy first; an extraction failure must not select a different interpreter.
pub fn provision() -> Result<Option<Runtime>> {
    if !available() {
        return Ok(None);
    }
    provision_in(&std::env::temp_dir().canonicalize()?).map(Some)
}

pub(crate) fn provision_in(parent: &Path) -> Result<Runtime> {
    ensure!(available(), "this build carries no CPython");
    extract(parent, payload::PAYLOAD, payload::SIZE)
        .with_context(|| format!("extracting bundled CPython {}", payload::SHA256))
}

fn extract(parent: &Path, gz: &[u8], size: u64) -> Result<Runtime> {
    let mut staging = tempfile::Builder::new();
    staging.prefix("dial-python-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A shared non-sticky temp parent would let another user rename the entire tree.
        let mode = fs::metadata(parent)?.permissions().mode();
        ensure!(
            (crate::platform::fs::path_owned_by_current_user(parent)?
                || crate::platform::fs::path_owned_by_system(parent)?)
                && (mode & 0o022 == 0 || mode & 0o1000 != 0),
            "untrusted CPython staging directory"
        );
        staging.permissions(fs::Permissions::from_mode(0o700));
    }
    let tree = staging.tempdir_in(parent)?;
    crate::platform::atomic::private_dir(tree.path())?;
    let raw = flate2::read::GzDecoder::new(gz).take(size);
    let mut tar = tar::Archive::new(raw);
    let mut remaining = size;
    for (i, entry) in tar.entries()?.enumerate() {
        ensure!(i < 32_768, "too many bundled runtime files");
        let mut entry = entry?;
        ensure!(
            entry.header().entry_type().is_file(),
            "non-file in runtime payload"
        );
        ensure!(
            !entry.path_bytes().iter().any(|c| matches!(c, b'\\' | b':')),
            "nonportable path in runtime payload"
        );
        let name =
            super::prebuilt::inside_top(&entry.path()?, false)?.context("empty runtime path")?;
        let target = tree.path().join(name);
        fs::create_dir_all(target.parent().context("no runtime file parent")?)?;
        let mut output = fs::File::create_new(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        remaining = remaining
            .checked_sub(entry.size())
            .context("oversize runtime payload")?;
        let count = io::copy(&mut entry, &mut output)?;
        ensure!(count == entry.size(), "truncated runtime file");
        #[cfg(unix)]
        if entry.header().mode()? & 0o111 != 0 {
            crate::platform::fs::set_mode(&target, 0o700)?;
        }
    }
    let executable = tree.path().join(PROGRAM);
    ensure!(
        executable.is_file() && crate::platform::fs::is_executable(&executable),
        "missing CPython executable"
    );
    Ok(Runtime { tree, executable })
}

#[cfg(test)]
mod tests;
