//! Cross-process admission leases keep installed Python code stable during requests/publication.
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

pub(super) struct Lease(File);

impl Drop for Lease {
    fn drop(&mut self) {
        // CLOEXEC is not enough: another thread may fork while this lease is held.
        // Release the open-file-description lock even if a child has not exec'd yet.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn file(root: &Path, name: &str) -> Result<File> {
    let directory = root.join(".locks");
    fs::create_dir_all(&directory).context("creating extension lease directory")?;
    let name = crate::hex(name.as_bytes());
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{name}.lock")))
        .context("opening extension lease")
}

pub(super) fn shared(root: &Path, name: &str) -> Result<Lease> {
    let file = file(root, name)?;
    fs2::FileExt::try_lock_shared(&file).context(
        "extension installation is changing; retry after the management operation finishes",
    )?;
    Ok(Lease(file))
}

pub(super) fn exclusive(root: &Path, name: &str) -> Result<Lease> {
    let file = file(root, name)?;
    fs2::FileExt::try_lock_exclusive(&file).context(
        "extension is busy or another management operation is active; retry after it finishes",
    )?;
    Ok(Lease(file))
}

#[cfg(test)]
mod tests;
