#!/usr/bin/env python3
"""Exercise Make's build/install contract without compiling or touching the real install."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
BINARY = b'#!/bin/sh\nprintf "dial test-build\\n"\n'


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="dial-build-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copy2(ROOT / "Makefile", self.root / "Makefile")
        self.home = self.root / "home"
        self.install = self.root / "installed"
        self.install.mkdir()
        self.cache = self.root / "cache with spaces"
        self.cargo = self.root / "fake-cargo"
        self.cargo.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, sys\n"
            "root = pathlib.Path.cwd()\n"
            "(root / 'args.json').write_text(json.dumps(sys.argv[1:]))\n"
            "if os.environ.get('FAIL_BUILD'):\n"
            "    sys.exit(42)\n"
            "profile = sys.argv[sys.argv.index('--profile') + 1]\n"
            "directory = 'debug' if profile == 'dev' else profile\n"
            "out = pathlib.Path(os.environ['CARGO_TARGET_DIR']) / directory / 'dial'\n"
            "out.parent.mkdir(parents=True, exist_ok=True)\n"
            f"out.write_bytes({BINARY!r})\n"
            "out.chmod(0o755)\n"
        )
        self.cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.install}{os.pathsep}{os.environ['PATH']}")
        # Do not inherit the caller's feature selection or build cache.
        for name in ("MAKEFLAGS", "MFLAGS", "CARGO_TARGET_DIR", "SLIM", "FEATURES", "PROFILE"):
            self.env.pop(name, None)

    def make(self, *args, success=True):
        result = subprocess.run(
            ["make", f"CARGO={self.cargo}", f"HOME={self.home}",
             f"INSTALL_DIR={self.install}", f"CARGO_TARGET_DIR={self.cache}", *args],
            cwd=self.root, env=self.env, capture_output=True, text=True,
        )
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def arguments(self):
        return json.loads((self.root / "args.json").read_text())

    def test_default_update_uses_persistent_cache_and_preserves_backups(self):
        cargo_bin = self.home / ".cargo/bin/dial"
        cargo_bin.parent.mkdir(parents=True)
        targets = [self.root / "bin/dial", self.install / "dial", cargo_bin]
        for path in targets:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"old executable")
            path.chmod(0o755)
        self.make("update")
        self.assertEqual(self.arguments(), ["build", "--bin", "dial", "--profile", "local", "--locked", "--timings"])
        for path in targets:
            self.assertEqual(path.read_bytes(), BINARY)
            backups = list(path.parent.glob(path.name + ".bak-*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), b"old executable")
        self.assertEqual((self.cache / "local/dial").read_bytes(), BINARY)

    def test_slim_features_and_dev_install_path(self):
        self.make("update", "SLIM=1", "FEATURES=aws,desktop", "PROFILE=dev")
        self.assertEqual(self.arguments(), ["build", "--bin", "dial", "--profile", "dev", "--locked", "--timings", "--no-default-features", "--features", "aws,desktop"])
        self.assertEqual((self.install / "dial").read_bytes(), BINARY)
        self.assertEqual((self.cache / "debug/dial").read_bytes(), BINARY)

    def test_build_failure_leaves_install_untouched(self):
        installed = self.install / "dial"
        installed.write_bytes(b"keep me")
        self.env["FAIL_BUILD"] = "1"
        self.make("update", success=False)
        self.assertEqual(installed.read_bytes(), b"keep me")
        self.assertFalse((self.root / "bin").exists())
        self.assertEqual(list(self.install.glob("dial.bak-*")), [])

    def test_slim_build_does_not_install(self):
        self.make("build", "SLIM=1")
        self.assertIn("--no-default-features", self.arguments())
        self.assertFalse((self.install / "dial").exists())
        self.assertEqual((self.cache / "local/dial").read_bytes(), BINARY)


if __name__ == "__main__":
    unittest.main()
