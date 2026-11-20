//! Common process fixture for the GitHub and GitLab CLI runners. Each runner installs its
//! own thread-local override; this module only supplies the executable and recorded arguments.
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn fake_cli(
    name: &str,
    stdout: &str,
    stderr: &str,
    status: i32,
) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("stdout"), stdout).unwrap();
    fs::write(root.join("stderr"), stderr).unwrap();
    let script = format!(
        "#!/bin/sh\ndir=${{0%/*}}\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done >> \"$dir/argv\"\nprintf '\\036' >> \"$dir/argv\"\ncat \"$dir/stdout\"\ncat \"$dir/stderr\" >&2\nexit {status}\n"
    );
    let bin = root.join(name);
    fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, bin)
}

pub(super) fn calls(root: &Path) -> Vec<Vec<String>> {
    let raw = fs::read(root.join("argv")).unwrap_or_default();
    raw.split(|&b| b == 0x1e)
        .filter(|record| !record.is_empty())
        .map(|record| {
            record
                .strip_suffix(&[0])
                .unwrap()
                .split(|&b| b == 0)
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect()
        })
        .collect()
}
