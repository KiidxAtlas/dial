//! Operating-system layer: the primitives whose implementation differs between Unix and Windows.
//!
//! Unix implementations are exactly the syscalls Dial has always issued (`O_NOFOLLOW` opens,
//! `openat` walks, process groups and `killpg`, `O_NONBLOCK` pipes with `poll`). Windows
//! implementations are real equivalents, never no-ops that pretend to succeed:
//!
//! * symlink-safe opens use `FILE_FLAG_OPEN_REPARSE_POINT` and refuse name-surrogate reparse points
//!   (symbolic links, junctions); walks beneath a root use handle-relative `NtCreateFile`;
//! * owner/permission checks read the file's owner SID and DACL; "owner-only" installs a protected
//!   DACL granting only the current user;
//! * process groups are Job Objects (children are created suspended, assigned, then resumed, so no
//!   descendant can escape before containment);
//! * nonblocking pipe reads peek the pipe (`PeekNamedPipe`), so they work for overlapped and
//!   synchronous anonymous pipes alike.
//!
//! See `docs/windows.md` for user-facing behavior.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use imp::pty::{NativePtyChild, spawn_conpty};
#[cfg(windows)]
use windows as imp;

/// Device and file number: equal identities mean the same file object (`st_dev`/`st_ino`, or the
/// Windows volume serial number and 64-bit file index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Filesystem safety primitives.
pub mod fs;

/// Process groups: a leader and every descendant, signalled and killed together.
pub mod process;

pub mod atomic;
pub mod interrupt;
pub mod process_record;
pub mod procinfo;
/// Event-driven waits: pipe readiness, child exit, and cancellation wakeups.
pub mod wake;

/// Nonblocking pipe I/O.
pub mod io;

/// Time without a date library.
pub mod time;

/// Host facts and process-wide signal handling.
pub mod sys;
