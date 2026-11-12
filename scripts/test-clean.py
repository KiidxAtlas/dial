#!/usr/bin/env python3
"""Offline tests for `make clean` (scripts/clean.py); no compilation or network required."""
import fcntl
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().with_name("clean.py")
spec = importlib.util.spec_from_file_location("clean", SCRIPT)
clean = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clean)


def build_dir(path, profile="debug"):
    (path / profile / "deps").mkdir(parents=True)
    (path / profile / "deps" / "libx.rlib").write_bytes(b"x" * 4096)
    (path / profile / ".cargo-lock").write_bytes(b"")
    return path


def hold(path, mode=fcntl.LOCK_EX):
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.flock(fd, mode)
    return fd


class CleanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "src").mkdir()
        (self.root / "src" / "main.rs").write_text("fn main() {}\n")
        self.target = build_dir(self.root / "target")
        self.sessions = self.root / ".tmp" / "cargo-target"
        self.idle = build_dir(self.sessions / "idle1")
        self.live = build_dir(self.sessions / "live1")
        (self.sessions / ".leases").mkdir(parents=True)
        self.fds = []

    def tearDown(self):
        for fd in self.fds:
            os.close(fd)
        self.tmp.cleanup()

    def run_clean(self, **kwargs):
        out = io.StringIO()
        result = clean.clean(self.root, "target", out=out, **kwargs)
        return result, out.getvalue()

    def test_removes_idle_build_output_and_keeps_live_sessions_and_sources(self):
        # A live Dial session holds a shared lease on its directory.
        self.fds.append(hold(self.sessions / ".leases" / "live1", fcntl.LOCK_SH))
        (freed, skipped), out = self.run_clean()
        self.assertFalse(self.target.exists())
        self.assertFalse(self.idle.exists())
        self.assertTrue(self.live.exists())
        self.assertEqual(skipped, [self.live])
        self.assertGreater(freed, 0)
        self.assertTrue((self.root / "src" / "main.rs").exists())
        self.assertTrue((self.sessions / ".leases").is_dir(), "lease files are never removed")
        self.assertIn("in use", out)

    def test_a_running_cargo_build_keeps_its_target(self):
        self.fds.append(hold(self.target / "debug" / ".cargo-lock"))
        (_, skipped), _ = self.run_clean()
        self.assertEqual(skipped, [self.target])
        self.assertTrue(self.target.exists())
        self.assertFalse(self.idle.exists())

    def test_cross_target_profile_locks_are_honoured(self):
        cross = build_dir(self.idle / "x86_64-unknown-linux-musl", "release")
        self.fds.append(hold(cross / "release" / ".cargo-lock"))
        (_, skipped), _ = self.run_clean()
        self.assertIn(self.idle, skipped)
        self.assertTrue(self.idle.exists())

    def test_dry_run_removes_nothing(self):
        (freed, skipped), out = self.run_clean(dry_run=True)
        self.assertGreater(freed, 0)
        self.assertEqual(skipped, [])
        self.assertTrue(self.target.exists() and self.idle.exists() and self.live.exists())
        self.assertIn("would remove target", out)

    def test_symlinked_build_dirs_are_not_followed(self):
        outside = build_dir(self.root / "outside")
        os.symlink(outside, self.sessions / "linked")
        self.run_clean()
        self.assertTrue((outside / "debug" / "deps" / "libx.rlib").exists())

    def test_missing_directories_are_fine(self):
        empty = Path(self.tmp.name) / "empty"
        empty.mkdir()
        freed, skipped = clean.clean(empty, "target", out=io.StringIO())
        self.assertEqual((freed, skipped), (0, []))

    def test_a_session_exported_target_dir_still_cleans_target_and_respects_the_lease(self):
        # Inside Dial, CARGO_TARGET_DIR names the running session's own leased directory.
        self.fds.append(hold(self.sessions / ".leases" / "live1", fcntl.LOCK_SH))
        out = io.StringIO()
        _, skipped = clean.clean(self.root, str(self.live), out=out)
        self.assertFalse(self.target.exists(), "the checkout's target/ is still cleaned")
        self.assertEqual(skipped, [self.live])
        self.assertTrue(self.live.exists())


if __name__ == "__main__":
    unittest.main()
