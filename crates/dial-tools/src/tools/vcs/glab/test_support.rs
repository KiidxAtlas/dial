use std::cell::RefCell;
use std::ffi::OsString;
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
        let (dir, bin) = super::super::test_support::fake_cli("glab", stdout, stderr, status);
        Self {
            dir,
            _override: GlabOverride::install(&bin),
        }
    }

    /// Every recorded invocation, one `Vec` of arguments per call.
    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        super::super::test_support::calls(self.dir.path())
    }

    /// Arguments of the most recent call.
    pub(crate) fn last_args(&self) -> Vec<String> {
        self.calls().pop().unwrap_or_default()
    }
}
