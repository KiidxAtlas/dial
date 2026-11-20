use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use parking_lot::Mutex;

/// Serializes every test that mutates PATH (git fakes and HOME/PATH fixtures).
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
        let (dir, bin) = super::super::test_support::fake_cli("gh", stdout, stderr, status);
        Self {
            dir,
            _override: GhOverride::install(&bin),
        }
    }

    /// Every recorded invocation, one `Vec` of arguments per call (records are RS-terminated,
    /// arguments NUL-terminated).
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        super::super::test_support::calls(self.dir.path())
    }

    /// Arguments of the most recent call.
    pub(crate) fn last_args(&self) -> Vec<String> {
        self.calls().pop().unwrap_or_default()
    }
}
