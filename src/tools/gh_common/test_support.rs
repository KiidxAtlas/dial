use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

/// Serializes every test that mutates PATH (git fakes and HOME/PATH fixtures).
pub(crate) static PATH_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static GH_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The fake `gh` that `run_gh` spawns on this thread, if one is installed.
pub(super) fn gh_override() -> Option<OsString> {
    GH_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .map(PathBuf::into_os_string)
}

/// Routes `run_gh` on the calling thread to `program` until dropped. PATH is not touched.
pub(crate) struct GhOverride;

impl GhOverride {
    pub(crate) fn install(program: &Path) -> Self {
        GH_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(program.to_path_buf()));
        Self
    }
}

impl Drop for GhOverride {
    fn drop(&mut self) {
        GH_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

/// A `gh` script that answers every call made on this thread until dropped. Records argv.
pub(crate) struct FakeGh {
    dir: tempfile::TempDir,
    _override: GhOverride,
}

impl FakeGh {
    /// Install a `gh` that prints `stdout`, prints `stderr` to stderr, and exits `status`.
    pub(crate) fn new(stdout: &str, stderr: &str, status: i32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("stdout"), stdout).unwrap();
        fs::write(root.join("stderr"), stderr).unwrap();
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done >> '{argv}'\nprintf '\\036' >> '{argv}'\ncat '{out}'\ncat '{err}' >&2\nexit {status}\n",
            argv = root.join("argv").display(),
            out = root.join("stdout").display(),
            err = root.join("stderr").display(),
        );
        let bin = root.join("gh");
        fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Windows has no file mode bits, so the chmod step is skipped there; the fake is a
        // POSIX shell script and only runs on Unix.
        Self {
            dir,
            _override: GhOverride::install(&bin),
        }
    }

    /// Every recorded invocation, one `Vec` of arguments per call (records are RS-terminated,
    /// arguments NUL-terminated).
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        let raw = fs::read(self.dir.path().join("argv")).unwrap_or_default();
        raw.split(|b| *b == 0x1e)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let mut words: Vec<String> = record
                    .split(|b| *b == 0)
                    .map(|word| String::from_utf8_lossy(word).into_owned())
                    .collect();
                words.pop();
                words
            })
            .collect()
    }

    /// Arguments of the most recent call.
    pub(crate) fn last_args(&self) -> Vec<String> {
        self.calls().pop().unwrap_or_default()
    }
}
