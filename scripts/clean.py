#!/usr/bin/env python3
"""Remove Rust build output from this checkout without disturbing running work.

Removed:
  * the Cargo target directory (`CARGO_TARGET_DIR`, default `target/`);
  * per-session build directories under `.tmp/cargo-target/` (see docs/shell.md);
  * preflight's build cache, `.tmp/preflight/target`.

Each directory is removed only while no Cargo build holds one of its profile locks, and a
session directory only while no live Dial session holds its lease, so builds and sessions in
progress keep their output. Those are skipped and reported. Source files, git worktrees,
installed binaries and Cargo's download cache (~/.cargo/registry) are never touched.
"""
import argparse
import os
import shutil
import sys
from pathlib import Path

try:
    import fcntl
except ImportError:  # Windows: no flock; refuse rather than delete a directory in use.
    fcntl = None

SESSION_ROOT = Path(".tmp") / "cargo-target"
LEASES = ".leases"
# Same bound as Dial's own maintenance (crates/dial-tools/src/tools/exec/cargo_target.rs).
MAX_LAYOUT_ENTRIES = 1024


class Busy(Exception):
    """A lock is held by a running build or session."""


def try_lock(path, create=False):
    """An exclusive non-blocking flock on `path`, or None when the file does not exist."""
    flags = os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    if create:
        flags |= os.O_CREAT
    try:
        fd = os.open(path, flags, 0o600)
    except FileNotFoundError:
        return None
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        raise Busy(str(path))
    return fd


def cargo_locks(target):
    """Hold every profile lock of a Cargo target directory (`<profile>/.cargo-lock`, plus
    `<triple>/<profile>/.cargo-lock` for cross builds). Raises Busy while a build runs."""
    held, pending, entries = [], [(Path(target), 0)], 0
    try:
        while pending:
            path, depth = pending.pop()
            for entry in os.scandir(path):
                entries += 1
                if entries > MAX_LAYOUT_ENTRIES:
                    raise Busy(f"{target}: unexpected layout")
                if entry.is_symlink() or not entry.is_dir(follow_symlinks=False):
                    continue
                fd = try_lock(Path(entry.path) / ".cargo-lock")
                if fd is not None:
                    held.append(fd)
                elif depth == 0:
                    pending.append((Path(entry.path), 1))
    except BaseException:
        release(held)
        raise
    return held


def release(fds):
    for fd in fds:
        os.close(fd)


def size_of(path):
    total = 0
    for root, dirs, files in os.walk(path):
        for name in files + dirs:
            try:
                total += os.lstat(os.path.join(root, name)).st_blocks * 512
            except OSError:
                pass
    return total


def human(size):
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024 or unit == "GB":
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TB"


def candidates(root, target_dir):
    """(path, lease file or None) for every build directory this command may remove."""
    found = [(root / "target", None)]
    target = Path(target_dir)
    if not target.is_absolute():
        target = root / target
    sessions = root / SESSION_ROOT
    # Commands Dial runs export their session directory; that one is handled with its lease below.
    if target.resolve() != (root / "target").resolve() and target.parent.resolve() != sessions.resolve():
        found.append((target, None))
    if sessions.is_dir():
        for entry in sorted(os.scandir(sessions), key=lambda e: e.name):
            if entry.name == LEASES or entry.is_symlink() or not entry.is_dir():
                continue
            found.append((Path(entry.path), sessions / LEASES / entry.name))
    found.append((root / ".tmp" / "preflight" / "target", None))
    return [(path, lease) for path, lease in found if path.is_dir() and not path.is_symlink()]


def clean(root, target_dir, dry_run=False, out=sys.stdout):
    """Remove idle build directories; returns (bytes freed or freeable, skipped paths)."""
    freed, skipped = 0, []
    for path, lease in candidates(root, target_dir):
        held = []
        try:
            if lease is not None:
                lease.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                held.append(try_lock(lease, create=True))
            held.extend(cargo_locks(path))
        except Busy:
            release(held)
            skipped.append(path)
            print(f"skip    {path.relative_to(root) if path.is_relative_to(root) else path}"
                  " (in use by a running build or Dial session)", file=out)
            continue
        try:
            size = size_of(path)
            shown = path.relative_to(root) if path.is_relative_to(root) else path
            if dry_run:
                print(f"would remove {shown} ({human(size)})", file=out)
            else:
                shutil.rmtree(path)
                print(f"removed {shown} ({human(size)})", file=out)
            freed += size
        finally:
            release(held)
    verb = "Would free" if dry_run else "Freed"
    print(f"{verb} {human(freed)}." + (f" Skipped {len(skipped)} in use; run again once they finish."
                                     if skipped else ""), file=out)
    return freed, skipped


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--target-dir", default=os.environ.get("CARGO_TARGET_DIR") or "target")
    parser.add_argument("--dry-run", action="store_true", help="list what would be removed")
    args = parser.parse_args()
    if fcntl is None:
        print("make clean needs file locking (macOS, Linux or WSL); remove target/ by hand.",
              file=sys.stderr)
        return 2
    root = Path(__file__).resolve().parent.parent
    clean(root, args.target_dir, args.dry_run)
    return 0


if __name__ == "__main__":
    sys.exit(main())
