#!/usr/bin/env python3
"""Exercise Make's build/install contract without compiling or touching the real install."""
import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = b'#!/bin/sh\nprintf "dial test-build\\n"\n'


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="dial-build-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        shutil.copy2(ROOT / "Makefile", self.root / "Makefile")
        (self.root / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts" / "install-dev.py", self.root / "scripts" / "install-dev.py")
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
        self.assertEqual(self.arguments(), ["build", "-p", "dial", "--bin", "dial", "--profile", "local", "--locked", "--timings"])
        for path in targets:
            self.assertEqual(path.read_bytes(), BINARY)
            backups = list(path.parent.glob(path.name + ".bak-*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_bytes(), b"old executable")
        self.assertEqual((self.cache / "local/dial").read_bytes(), BINARY)

    def test_slim_features_and_dev_install_path(self):
        self.make("update", "SLIM=1", "FEATURES=aws,desktop", "PROFILE=dev")
        self.assertEqual(self.arguments(), ["build", "-p", "dial", "--bin", "dial", "--profile", "dev", "--locked", "--timings", "--no-default-features", "--features", "aws,desktop"])
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


    def test_prune_keeps_newest_backups_only_after_a_successful_update(self):
        installed = self.install / "dial"
        installed.write_bytes(b"old executable")
        installed.chmod(0o755)
        for index in range(5):
            (self.install / f"dial.bak-20000101-00000{index}").write_bytes(b"history")
        self.make("update", "KEEP_BACKUPS=2")
        self.assertEqual(installed.read_bytes(), BINARY)
        remaining = sorted(path.name for path in self.install.glob("dial.bak-*"))
        self.assertEqual(len(remaining), 2, remaining)
        self.assertNotIn("dial.bak-20000101-000000", remaining)
        self.assertIn("dial.bak-20000101-000004", remaining)

    def test_invalid_retention_fails_before_building(self):
        for keep in ("0", "many", ""):
            with self.subTest(keep=keep):
                self.make("update", f"KEEP_BACKUPS={keep}", success=False)
                self.assertFalse((self.root / "args.json").exists())
        self.assertFalse((self.install / "dial").exists())

    def test_failed_backup_stops_before_any_binary_is_replaced(self):
        if hasattr(os, "geteuid") and os.geteuid() == 0:
            self.skipTest("root bypasses directory permissions, so the backup cannot be blocked")
        installed = self.install / "dial"
        installed.write_bytes(b"old executable")
        installed.chmod(0o755)
        self.install.chmod(0o555)
        self.addCleanup(self.install.chmod, 0o755)
        self.make("update", success=False)
        self.assertEqual(installed.read_bytes(), b"old executable")
        self.assertFalse((self.root / "bin/dial").exists())

    def test_failed_path_check_keeps_older_backups(self):
        installed = self.install / "dial"
        installed.write_bytes(b"old executable")
        installed.chmod(0o755)
        history = [self.install / f"dial.bak-20000101-00000{index}" for index in range(5)]
        for path in history:
            path.write_bytes(b"history")
        other = self.root / "other"
        other.mkdir()
        (other / "dial").write_bytes(b"#!/bin/sh\nprintf 'other dial\\n'\n")
        (other / "dial").chmod(0o755)
        self.env["PATH"] = f"{other}{os.pathsep}{self.env['PATH']}"
        self.make("update", "KEEP_BACKUPS=2", success=False)
        for path in history:
            self.assertTrue(path.exists(), f"{path.name} was pruned before activation was verified")


class InstallDevTests(unittest.TestCase):
    """Unit tests for scripts/install-dev.py fault paths that Make cannot inject."""

    def setUp(self):
        spec = importlib.util.spec_from_file_location("install_dev", ROOT / "scripts" / "install-dev.py")
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.temp = tempfile.TemporaryDirectory(prefix="dial-install-dev-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.build = self.root / "build-dial"
        self.build.write_bytes(BINARY)
        self.bin = self.root / "bin/dial"
        self.install = self.root / "installed/dial"
        self.cargo = self.root / "cargo/bin/dial"
        for path, content in ((self.bin, b"old bin"), (self.install, b"old install"), (self.cargo, b"old cargo")):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.destinations = [self.bin, self.install, self.cargo]

    def test_failed_second_activation_restores_the_first_location(self):
        calls = []

        def failing_replace(source, destination):
            calls.append(destination)
            if len(calls) == 2:
                raise OSError("injected activation failure")
            os.replace(source, destination)

        with self.assertRaisesRegex(self.module.InstallError, "activating .* failed"):
            self.module.install(self.build, self.destinations, "20250101-000000", replace=failing_replace)
        self.assertEqual(self.bin.read_bytes(), b"old bin")
        self.assertEqual(self.install.read_bytes(), b"old install")
        self.assertEqual(self.cargo.read_bytes(), b"old cargo")
        self.assertEqual(list(self.root.rglob(".dial.new-*")), [])

    def test_post_install_mismatch_restores_every_location(self):
        def corrupting_replace(source, destination):
            os.replace(source, destination)
            if destination == self.cargo:
                destination.write_bytes(b"corrupt")

        with self.assertRaisesRegex(self.module.InstallError, "does not match the build"):
            self.module.install(self.build, self.destinations, "20250101-000000", replace=corrupting_replace)
        self.assertEqual(self.bin.read_bytes(), b"old bin")
        self.assertEqual(self.install.read_bytes(), b"old install")
        self.assertEqual(self.cargo.read_bytes(), b"old cargo")

    def test_post_install_verification_exception_restores_every_location(self):
        compare = self.module.filecmp.cmp

        def failing_compare(source, destination, **kwargs):
            if source == self.build and destination == self.install:
                raise OSError("injected verification read failure")
            return compare(source, destination, **kwargs)

        with mock.patch.object(self.module.filecmp, "cmp", side_effect=failing_compare):
            with self.assertRaisesRegex(self.module.InstallError, "verifying .*injected verification read failure") as raised:
                self.module.install(self.build, self.destinations, "20250101-000000")
        self.assertIsInstance(raised.exception.__cause__, OSError)
        self.assertIn("previous binaries restored", str(raised.exception))
        self.assertEqual(self.bin.read_bytes(), b"old bin")
        self.assertEqual(self.install.read_bytes(), b"old install")
        self.assertEqual(self.cargo.read_bytes(), b"old cargo")

    def test_post_install_failure_removes_new_destinations(self):
        compare = self.module.filecmp.cmp
        for failure in (False, OSError("injected verification failure")):
            with self.subTest(failure=failure):
                self.install.unlink(missing_ok=True)

                def failing_compare(source, destination, **kwargs):
                    if source == self.build and destination == self.cargo:
                        if isinstance(failure, OSError):
                            raise failure
                        return failure
                    return compare(source, destination, **kwargs)

                with mock.patch.object(self.module.filecmp, "cmp", side_effect=failing_compare):
                    with self.assertRaises(self.module.InstallError):
                        self.module.install(self.build, self.destinations, "20250101-000000")
                self.assertFalse(self.install.exists())
                self.assertEqual(self.bin.read_bytes(), b"old bin")
                self.assertEqual(self.cargo.read_bytes(), b"old cargo")

    def test_verification_and_restoration_errors_are_both_reported(self):
        compare = self.module.filecmp.cmp
        replace = os.replace

        def failing_compare(source, destination, **kwargs):
            if source == self.build:
                raise OSError("injected verification failure")
            return compare(source, destination, **kwargs)

        def failing_restore(source, destination):
            if destination == self.bin and ".bak-" in source.name:
                raise OSError("injected restoration failure")
            return replace(source, destination)

        with mock.patch.object(self.module.filecmp, "cmp", side_effect=failing_compare), \
                mock.patch.object(self.module.os, "replace", side_effect=failing_restore):
            with self.assertRaises(self.module.InstallError) as raised:
                self.module.install(self.build, self.destinations, "20250101-000000", replace=replace)
        message = str(raised.exception)
        self.assertIn("injected verification failure", message)
        self.assertIn(f"{self.bin}: injected restoration failure", message)
        self.assertNotIn("previous binaries restored", message)
        self.assertEqual(self.bin.read_bytes(), BINARY)
        self.assertEqual(self.bin.with_name("dial.bak-20250101-000000").read_bytes(), b"old bin")
        self.assertEqual(self.install.read_bytes(), b"old install")
        self.assertEqual(self.cargo.read_bytes(), b"old cargo")

    def test_colliding_timestamps_get_distinct_rollback_points(self):
        first = self.module.install(self.build, [self.install], "20250101-000000")
        self.install.write_bytes(b"second generation")
        second = self.module.install(self.build, [self.install], "20250101-000000")
        self.assertNotEqual(first[self.install], second[self.install])
        self.assertEqual(first[self.install].read_bytes(), b"old install")
        self.assertEqual(second[self.install].read_bytes(), b"second generation")
        self.assertEqual(second[self.install].name, "dial.bak-20250101-000000-02")

    def test_invalid_keep_is_rejected(self):
        for value in ("0", "-1", "3x", ""):
            with self.subTest(value=value):
                with self.assertRaises(self.module.InstallError):
                    self.module.positive_keep(value)


class CheckTargetTests(unittest.TestCase):
    def test_python_targets_use_selected_interpreter_and_stop_on_failure(self):
        targets = {
            "test-build": ["scripts/test-build.py"],
            "test-windows-contract": ["scripts/test-windows-contract.py"],
            "test-packaging": ["scripts/test-packaging.py", "scripts/check-packaging.py"],
            "test-release-policy": ["scripts/test-prerequisites.py release-policy", "scripts/test-release-policy.py",
                                    "scripts/test-benchmark-release.py"],
        }
        with tempfile.TemporaryDirectory(prefix="dial-check-targets-") as directory:
            root = Path(directory)
            shutil.copy2(ROOT / "Makefile", root / "Makefile")
            runner = root / "selected-python"
            runner.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$*\" >> calls\n"
                "[ \"${FAIL_CHECK:-0}\" != 1 ]\n"
            )
            runner.chmod(0o755)
            env = {key: value for key, value in os.environ.items()
                   if key not in ("MAKEFLAGS", "MFLAGS", "FAIL_CHECK")}
            for target, expected in targets.items():
                for fail in (False, True):
                    with self.subTest(target=target, fail=fail):
                        (root / "calls").unlink(missing_ok=True)
                        result = subprocess.run(
                            ["make", target, f"PYTHON={runner}"], cwd=root,
                            env=dict(env, FAIL_CHECK=str(int(fail))),
                            capture_output=True, text=True,
                        )
                        self.assertEqual(result.returncode == 0, not fail,
                                         result.stdout + result.stderr)
                        self.assertEqual((root / "calls").read_text().splitlines(),
                                         expected[:1] if fail else expected)


    def test_release_check_parses_both_shell_scripts_and_aggregates_failures(self):
        with tempfile.TemporaryDirectory(prefix="dial-release-check-") as directory:
            root = Path(directory)
            shutil.copy2(ROOT / "Makefile", root / "Makefile")
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "release.sh").write_text("exit 0\n")
            (scripts / "preflight.sh").write_text("if then\n")
            runner = root / "python-fixture"
            runner.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> calls\nexit "${FAIL_PYTHON:-0}"\n')
            runner.chmod(0o755)
            for failure in ("0", "1"):
                with self.subTest(failure=failure):
                    (root / "calls").unlink(missing_ok=True)
                    result = subprocess.run(["make", "release-check", f"PYTHON={runner}", "RELEASE_TAG="], cwd=root,
                                            env=dict(os.environ, FAIL_PYTHON=failure), capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("preflight.sh", result.stderr)
                    self.assertEqual((root / "calls").read_text().splitlines(),
                                     ["scripts/release-policy.py check", "scripts/check-packaging.py",
                                      "scripts/release-policy.py tools"])

    def test_preflight_stops_before_compilation_on_readiness_failure_even_with_keep_going(self):
        with tempfile.TemporaryDirectory(prefix="dial-preflight-readiness-") as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copy2(ROOT / "scripts/preflight.sh", scripts / "preflight.sh")
            (scripts / "release-policy.py").write_text("pass\n")
            (root / "Makefile").write_text('release-check:\n\t@echo readiness >> calls\n\t@exit 1\n')
            tools = root / "bin"
            tools.mkdir()
            stubs = {
                "git": f'echo "{root}"\n',
                "rustup": f'if [ "$1" = which ]; then echo "{tools}/cargo"; else echo aarch64-apple-darwin; fi\n',
                "rustc": "printf 'rustc fixture\\nhost: aarch64-apple-darwin\\n'\n",
                "cargo": 'echo "cargo $*" >> calls\nexit 97\n',
                "uv": 'while [ "$1" != --with-requirements ]; do shift; done\nshift 2\nexec "$@"\n',
            }
            for name, body in stubs.items():
                path = tools / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(0o755)
            env = dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}", PREFLIGHT_KEEP_GOING="1")
            for args in ([], ["--quick"]):
                with self.subTest(args=args):
                    (root / "calls").unlink(missing_ok=True)
                    result = subprocess.run(["bash", str(scripts / "preflight.sh"), "--in-place", *args],
                                            cwd=root, env=env, capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual((root / "calls").read_text().splitlines(), ["readiness"])

    def test_preflight_uses_selected_revision_dependencies_and_check_interface(self):
        with tempfile.TemporaryDirectory(prefix="dial-preflight-test-") as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copy2(ROOT / "scripts/preflight.sh", scripts / "preflight.sh")
            (scripts / "requirements-ci.txt").write_text("selected-legacy\n")
            (scripts / "test-install.sh").write_text("exit 0\n")
            for name in ("test-build", "test-windows-contract", "test-packaging",
                         "check-packaging", "test-release-policy", "interactive_smoke",
                         "interactive_surfaces", "session_contention_smoke", "side_lifecycle_smoke"):
                path = (scripts if "smoke" not in name and name != "interactive_surfaces"
                        else root / "examples/benchmarks") / f"{name}.py"
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("pass\n")
            tiers = "test-drivers test-python test-bench-offline test-nextest test-rust test-smoke"
            (root / "Makefile").write_text(f".PHONY: {tiers}\n{tiers}:\n\t@true\n")
            env = dict(os.environ, GIT_AUTHOR_NAME="test", GIT_AUTHOR_EMAIL="test@example.invalid",
                       GIT_COMMITTER_NAME="test", GIT_COMMITTER_EMAIL="test@example.invalid")
            for key in ("MAKEFLAGS", "MFLAGS", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
                env.pop(key, None)

            def git(*args):
                return subprocess.check_output(
                    ["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false", *args],
                    cwd=root, env=env, text=True, stderr=subprocess.STDOUT,
                ).strip()

            git("init", "-q")
            git("add", "Makefile", "scripts", "examples")
            git("commit", "-qm", "legacy check interface")
            legacy = git("rev-parse", "HEAD")
            (scripts / "requirements-ci.lock").write_text("selected-lock\n")
            shared = "test-install test-build test-windows-contract test-packaging test-release-policy"
            with (root / "Makefile").open("a") as handle:
                handle.write(f".PHONY: {shared}\n{shared}:\n\t@true\n")
            git("add", "Makefile", "scripts/requirements-ci.lock")
            git("commit", "-qm", "shared check interface")
            modern = git("rev-parse", "HEAD")
            # The launching checkout must not supply dependencies for either selected revision.
            (scripts / "requirements-ci.lock").write_text("wrong-current-lock\n")
            (scripts / "requirements-ci.txt").write_text("wrong-current-direct\n")
            tools = root / "fake-tools"
            tools.mkdir()
            stubs = {
                "cargo": "exit 0\n",
                "rustup": f"if [ \"$1\" = which ]; then echo '{tools}/cargo'; else echo aarch64-apple-darwin; fi\n",
                "rustc": "printf 'rustc test\\nhost: aarch64-apple-darwin\\n'\n",
                "uv": 'while [ "$1" != --with-requirements ]; do shift; done\n'
                      'cat "$2" >> "$PREFLIGHT_DEPENDENCY_LOG"\nshift 2\nexec "$@"\n',
            }
            for name, body in stubs.items():
                path = tools / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(0o755)
            log = root / "dependencies.log"
            env.update(PATH=f"{tools}{os.pathsep}{env['PATH']}", PREFLIGHT_DEPENDENCY_LOG=str(log))
            for revision, expected in ((legacy, "selected-legacy"), (modern, "selected-lock")):
                with self.subTest(revision=revision):
                    log.unlink(missing_ok=True)
                    result = subprocess.run(["bash", str(scripts / "preflight.sh"), "--force", revision],
                                            cwd=root, env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertGreater(len(log.read_text().splitlines()), 5)
                    self.assertEqual(set(log.read_text().splitlines()), {expected})


if __name__ == "__main__":
    unittest.main()
