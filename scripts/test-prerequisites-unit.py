#!/usr/bin/env python3
"""Offline coverage for the Linux prerequisite diagnosis and Make's fail-closed wiring."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("prerequisites", ROOT / "scripts/test-prerequisites.py")
prerequisites = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prerequisites)


class PrerequisiteTests(unittest.TestCase):
    def setUp(self):
        for name, value in [("getuid", 1000), ("geteuid", 1000), ("path.isfile", True), ("access", True)]:
            target, attribute = name.rsplit(".", 1) if "." in name else (None, name)
            owner = prerequisites.os.path if target == "path" else prerequisites.os
            patch = mock.patch.object(owner, attribute, return_value=value, create=True)
            patch.start()
            self.addCleanup(patch.stop)
        patch = mock.patch.object(prerequisites.subprocess, "run",
                                  return_value=subprocess.CompletedProcess([], 0, "", ""))
        self.probe = patch.start()
        self.addCleanup(patch.stop)

    def test_root_and_missing_bubblewrap_are_reported_together_without_a_probe(self):
        with mock.patch.object(prerequisites.os, "getuid", return_value=0), \
                mock.patch.object(prerequisites.os.path, "isfile", return_value=False), \
                mock.patch.object(prerequisites.sys, "platform", "linux"), \
                contextlib.redirect_stderr(io.StringIO()) as error:
            self.assertEqual(prerequisites.main(), 1)
        diagnostic = error.getvalue()
        for text in ["unprivileged user", "/usr/bin/bwrap", "No Rust tests ran", "not a sandbox-test skip"]:
            self.assertIn(text, diagnostic)
        self.probe.assert_not_called()

    def test_real_or_effective_root_and_nonexecutable_bubblewrap_fail_closed(self):
        for uid, euid, executable in [(0, 1000, True), (1000, 0, True), (1000, 1000, False)]:
            with self.subTest(uid=uid, euid=euid, executable=executable), \
                    mock.patch.object(prerequisites.os, "getuid", return_value=uid), \
                    mock.patch.object(prerequisites.os, "geteuid", return_value=euid), \
                    mock.patch.object(prerequisites.os, "access", return_value=executable):
                self.assertTrue(prerequisites.linux_problems())
        self.probe.assert_not_called()

    def test_installed_bubblewrap_must_actually_create_namespaces(self):
        self.probe.return_value = subprocess.CompletedProcess([], 1, "", "namespace denied")
        problems = prerequisites.linux_problems()
        self.assertEqual(len(problems), 1)
        self.assertIn("namespace denied", problems[0])
        self.assertIn("seccomp/AppArmor", problems[0])
        self.assertIn("unprivileged user namespaces", problems[0])

    def test_probe_start_failure_and_timeout_are_actionable_failures(self):
        for error in [OSError("cannot execute"), subprocess.TimeoutExpired("bwrap", 10)]:
            with self.subTest(error=error):
                self.probe.side_effect = error
                problems = prerequisites.linux_problems()
                self.assertEqual(len(problems), 1)
                self.assertIn("could not complete", problems[0])
                self.assertIn("host/container security policy", problems[0])

    def test_available_linux_runs_a_bounded_readonly_namespace_probe(self):
        self.assertEqual(prerequisites.linux_problems(), [])
        args, options = self.probe.call_args
        self.assertEqual(args[0][0], "/usr/bin/bwrap")
        self.assertIn("--unshare-user", args[0])
        self.assertIn("--unshare-net", args[0])
        self.assertEqual(args[0][-2:], ["--", "/bin/true"])
        self.assertGreater(options["timeout"], 0)
        self.assertNotIn("HOME", options["env"])

    def test_nonlinux_does_not_probe_or_claim_sandbox_verification(self):
        with mock.patch.object(prerequisites.sys, "platform", "darwin"), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(prerequisites.main(), 0)
        self.assertIn("not applicable", output.getvalue())
        self.assertIn("native Rust checks remain required", output.getvalue())
        self.probe.assert_not_called()

    def test_missing_python_modules_name_the_interpreter_and_pinned_setup(self):
        with mock.patch.object(prerequisites.importlib, "import_module", side_effect=ImportError("missing")), \
                contextlib.redirect_stderr(io.StringIO()) as error:
            self.assertEqual(prerequisites.main("python"), 1)
        diagnostic = error.getvalue()
        self.assertIn(sys.executable, diagnostic)
        self.assertIn("pytest", diagnostic)
        self.assertIn("scripts/requirements-ci.lock", diagnostic)
        self.assertIn("No tests ran", diagnostic)
        self.probe.assert_not_called()

    def test_python_tiers_probe_only_their_required_imports(self):
        for suite, module in [("python", "pytest"), ("release-policy", "cryptography.hazmat.primitives.asymmetric.ed25519")]:
            with self.subTest(suite=suite), \
                    mock.patch.object(prerequisites.importlib, "import_module") as imported, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(prerequisites.main(suite), 0)
                imported.assert_called_once_with(module)
        self.probe.assert_not_called()

    def test_broken_python_import_is_a_prerequisite_failure(self):
        with mock.patch.object(prerequisites.importlib, "import_module", side_effect=OSError("shared library unavailable")), \
                contextlib.redirect_stderr(io.StringIO()) as error:
            self.assertEqual(prerequisites.main("release-policy"), 1)
        self.assertIn("shared library unavailable", error.getvalue())
        self.assertIn("cryptography", error.getvalue())


@unittest.skipIf(os.name == "nt", "Make's POSIX command fixtures require a Unix host")
class MakePrerequisiteTests(unittest.TestCase):
    def test_python_tiers_stop_before_running_tests_when_prerequisites_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copy2(ROOT / "Makefile", root / "Makefile")
            (root / "scripts").mkdir()
            (root / "scripts/test-prerequisites.py").write_text(
                "import sys\nprint('prerequisite gate')\nsys.exit(19)\n")
            for target in ["test-python", "test-release-policy"]:
                with self.subTest(target=target):
                    result = subprocess.run(
                        ["make", target, f"PYTHON={sys.executable}"], cwd=root,
                        capture_output=True, text=True, timeout=10)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("prerequisite gate", result.stdout)
                    self.assertNotIn("No module named", result.stderr)
                    self.assertNotIn("can't open file", result.stderr)

    def test_both_test_runners_stop_before_cargo_when_prerequisites_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copy2(ROOT / "Makefile", root / "Makefile")
            (root / "scripts").mkdir()
            (root / "scripts/test-prerequisites.py").write_text(
                "import os, sys\n"
                "open('checked', 'a').write('gate\\n')\n"
                "sys.exit(int(os.environ['GATE_EXIT']))\n")
            cargo = root / "fake-cargo"
            cargo.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" >> cargo-ran\n")
            cargo.chmod(0o755)
            nextest = root / "cargo-nextest"
            nextest.write_text("#!/bin/sh\nexit 0\n")
            nextest.chmod(0o755)
            env = dict(os.environ, PATH=f"{root}{os.pathsep}{os.environ['PATH']}")
            for key in ["MAKEFLAGS", "MFLAGS"]:
                env.pop(key, None)
            for target in ["test-rust", "test-nextest"]:
                for code in [19, 0]:
                    with self.subTest(target=target, prerequisite_exit=code):
                        env["GATE_EXIT"] = str(code)
                        (root / "checked").unlink(missing_ok=True)
                        (root / "cargo-ran").unlink(missing_ok=True)
                        result = subprocess.run(
                            ["make", target, f"CARGO={cargo}", f"PYTHON={sys.executable}"],
                            cwd=root, env=env, capture_output=True, text=True, timeout=10)
                        self.assertEqual((root / "checked").read_text(), "gate\n")
                        self.assertEqual(result.returncode == 0, code == 0, result.stdout + result.stderr)
                        if code:
                            self.assertFalse((root / "cargo-ran").exists())
                        else:
                            commands = (root / "cargo-ran").read_text().splitlines()
                            self.assertIn("--no-fail-fast", commands[0])
                            self.assertIn("--locked", commands[0])


if __name__ == "__main__":
    unittest.main()
