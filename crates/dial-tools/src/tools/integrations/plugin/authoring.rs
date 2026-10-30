//! Binary authoring commands use the same bundled Python SDK as installed extensions.
use super::installer::python::SDK;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs,
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
    time::Duration,
};

/// Create a starter extension. Existing destinations are never overwritten.
pub fn create(directory: &Path, name: Option<&str>, python: &Path) -> Result<String> {
    let mut args = vec![OsString::from("new"), directory.as_os_str().to_owned()];
    if let Some(name) = name {
        args.extend(["--name".into(), name.into()]);
    }
    run(&args, python)
}

/// Check manifest and Python syntax without importing extension code or installing dependencies.
pub fn check(directory: &Path, python: &Path) -> Result<String> {
    run(
        &[OsString::from("check"), directory.as_os_str().to_owned()],
        python,
    )
}

fn run(args: &[impl AsRef<OsStr>], python: &Path) -> Result<String> {
    let temp = tempfile::tempdir().context("staging bundled extension authoring SDK")?;
    let package = temp.path().join("dial_extensions");
    fs::create_dir(&package)?;
    for (name, source) in SDK {
        fs::write(package.join(name), source)?;
    }
    let bootstrap = temp.path().join("author.py");
    fs::write(
        &bootstrap,
        "import sys\nfrom pathlib import Path\nsys.path.insert(0, str(Path(__file__).resolve().parent))\nfrom dial_extensions.__main__ import main\nraise SystemExit(main())\n",
    )?;
    let mut command = Command::new(python);
    command.arg("-I").arg(&bootstrap).args(args);
    let timeout = Duration::from_secs(30);
    let output = crate::runtime::process::run(
        command,
        None,
        timeout,
        128 * 1024,
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .map_err(anyhow::Error::msg)
    .context(
        "running extension authoring helper; select an installed Python 3.11+ with --python",
    )?;
    ensure!(
        output.success(),
        "{}",
        output.failure("extension authoring", timeout)
    );
    ensure!(
        !output.output_incomplete,
        "extension authoring output was incomplete"
    );
    String::from_utf8(output.stdout).context("extension authoring returned invalid UTF-8")
}

#[cfg(test)]
mod tests;
