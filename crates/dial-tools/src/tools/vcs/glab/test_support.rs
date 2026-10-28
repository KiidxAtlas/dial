use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

thread_local! {
    static GLAB_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The fake `glab` that `glab_in` spawns on this thread, if one is installed.
pub(super) fn glab_override() -> Option<OsString> {
    GLAB_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .map(PathBuf::into_os_string)
}

/// Routes `glab_in` on the calling thread to `program` until dropped. PATH is not touched.
pub(crate) struct GlabOverride;

impl GlabOverride {
    pub(crate) fn install(program: &Path) -> Self {
        GLAB_OVERRIDE.with(|slot| *slot.borrow_mut() = Some(program.to_path_buf()));
        Self
    }
}

impl Drop for GlabOverride {
    fn drop(&mut self) {
        GLAB_OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

/// A `glab` script that answers every call made on this thread until dropped. Records argv.
pub(crate) struct FakeGlab {
    dir: tempfile::TempDir,
    _override: GlabOverride,
}

impl FakeGlab {
    /// Install a `glab` that prints `stdout`, prints `stderr` to stderr, and exits `status`.
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
        let bin = root.join("glab");
        fs::write(&bin, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The fake is a POSIX shell script; Windows has no mode bits and these tests are Unix-only.
        Self {
            dir,
            _override: GlabOverride::install(&bin),
        }
    }

    /// Every recorded invocation, one `Vec` of arguments per call.
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
