#!/usr/bin/env python3
"""Contract tests for scripts/embedded-host.py and its Makefile wiring; no Cargo build runs here.

Cargo, the host smoke check and the test command are mocked at subprocess level, so these tests pin
the command and environment contract only. They do not prove the embedded shell runs; that needs a
real `make test-*` run on a host with the toolchain.
"""
import contextlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("embedded_host", ROOT / "scripts" / "embedded-host.py")
host = importlib.util.module_from_spec(spec)
spec.loader.exec_module(host)


def artifact(path, name="dial", kind=("bin",)):
    return json.dumps({"reason": "compiler-artifact", "target": {"name": name, "kind": list(kind)},
                       "executable": path})


class FakeRun:
    """Stands in for subprocess.run: answers cargo and the host smoke check; records every call."""

    def __init__(self, executable, build_stdout=None, smoke_stdout=None, smoke_code=0, build_code=0):
        self.executable = executable
        self.build_stdout = build_stdout if build_stdout is not None else artifact(executable) + "\n"
        self.smoke_stdout = host.MARKER + "\n" if smoke_stdout is None else smoke_stdout
        self.smoke_code = smoke_code
        self.build_code = build_code
        self.calls = []

    def __call__(self, command, **kwargs):
        self.calls.append((list(command), kwargs))
        if command[0] == self.executable:
            return subprocess.CompletedProcess(command, self.smoke_code, stdout=self.smoke_stdout, stderr="")
        if command[1] == "build":
            return subprocess.CompletedProcess(command, self.build_code, stdout=self.build_stdout)
        return subprocess.CompletedProcess(command, 0, stdout="", stderr="")


class EmbeddedEnabledTests(unittest.TestCase):
    def test_feature_selection_decides_whether_the_host_exists(self):
        table = [
            (False, [], True),                        # default features include embedded-shell
            (True, [], False),                        # SLIM=1 without FEATURES
            (True, ["aws,desktop"], False),           # SLIM=1 with other features
            (True, ["aws,embedded-shell"], True),     # SLIM=1 re-enabling the shell
            (True, ["dial-tools/embedded-shell"], True),
            (True, ["embedded-shell-extra"], False),  # not the same feature name
        ]
        for no_default, features, expected in table:
            with self.subTest(no_default=no_default, features=features):
                self.assertIs(host.embedded_enabled(no_default, features), expected)


class BuildAndArtifactTests(unittest.TestCase):
    def test_build_command_mirrors_profile_and_features(self):
        self.assertEqual(
            host.build_command("cargo", "ci", False, []),
            ["cargo", "build", "--locked", "-p", "dial", "--bin", "dial",
             "--message-format=json-render-diagnostics", "--profile", "ci"])
        self.assertEqual(
            host.build_command("cargo", None, True, ["aws,embedded-shell"]),
            ["cargo", "build", "--locked", "-p", "dial", "--bin", "dial",
             "--message-format=json-render-diagnostics", "--no-default-features",
             "--features", "aws,embedded-shell"])

    def test_executable_comes_from_the_dial_bin_artifact_only(self):
        with tempfile.TemporaryDirectory() as directory:
            exe = str(Path(directory) / "custom-target" / "ci" / "dial")
            stdout = "\n".join([
                "warning: not json",
                artifact(str(Path(directory) / "other" / "dial"), name="dial-llm", kind=("lib",)),
                artifact(str(Path(directory) / "test" / "dial"), name="integration", kind=("test",)),
                json.dumps({"reason": "compiler-message", "message": {}}),
                artifact(exe),
            ])
            self.assertEqual(host.executable_from_messages(stdout), exe)

    def test_missing_or_ambiguous_artifacts_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            first = str(Path(directory) / "a" / "dial")
            second = str(Path(directory) / "b" / "dial")
            for stdout in ("", artifact(first, name="dial-llm", kind=("lib",)),
                           artifact(first) + "\n" + artifact(second)):
                with self.subTest(stdout=stdout), self.assertRaises(SystemExit):
                    host.executable_from_messages(stdout)


class PrepareTests(unittest.TestCase):
    def prepare(self, run, profile="ci", **kwargs):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            result = host.prepare("cargo", profile, run=run, out=out, **kwargs)
        return result, out.getvalue()

    def test_verified_fresh_build_returns_its_absolute_path(self):
        with tempfile.TemporaryDirectory() as directory:
            exe = str(Path(directory) / "target" / "ci" / "dial")
            run = FakeRun(exe)
            result, log = self.prepare(run, no_default_features=True, features=["aws,embedded-shell"])
        self.assertEqual(result, exe)
        build, build_kwargs = run.calls[0]
        self.assertEqual(build[:2], ["cargo", "build"])
        self.assertEqual(build[build.index("--profile") + 1], "ci")
        self.assertEqual(build[build.index("--features") + 1], "aws,embedded-shell")
        self.assertIn("--no-default-features", build)
        self.assertIsNotNone(build_kwargs.get("stdout"))
        smoke, _ = run.calls[1]
        self.assertEqual(smoke[0], exe)
        self.assertEqual(smoke[1], host.HOST_ARG)
        self.assertTrue(Path(smoke[2]).name.startswith("dial-shell-"))
        self.assertIn(f"embedded host: {exe}", log)

    def test_smoke_check_rejects_a_binary_that_does_not_answer_the_host(self):
        with tempfile.TemporaryDirectory() as directory:
            exe = str(Path(directory) / "dial")
            for run in (FakeRun(exe, smoke_stdout="usage: dial\n"), FakeRun(exe, smoke_code=2)):
                with self.subTest(code=run.smoke_code), self.assertRaises(SystemExit) as raised:
                    self.prepare(run)
                self.assertIn("smoke check failed", str(raised.exception))

    def test_failed_build_is_not_replaced_by_an_older_binary(self):
        run = FakeRun("/unused", build_code=101)
        with self.assertRaises(SystemExit) as raised:
            self.prepare(run)
        self.assertIn("build failed with exit 101", str(raised.exception))
        self.assertEqual(len(run.calls), 1)

    def test_dry_run_builds_and_executes_nothing(self):
        run = FakeRun("/unused")
        result, log = self.prepare(run, dry_run=True)
        self.assertIsNone(result)
        self.assertEqual(run.calls, [])
        self.assertIn("--profile ci", log)

    def test_no_embedded_shell_means_no_host_and_no_cargo_call(self):
        run = FakeRun("/unused")
        result, log = self.prepare(run, no_default_features=True, features=["aws"])
        self.assertIsNone(result)
        self.assertEqual(run.calls, [])
        self.assertIn("not in this build", log)


class EnvironmentTests(unittest.TestCase):
    def test_host_is_exported_and_stale_values_are_removed(self):
        base = {"PATH": "/bin", host.HOST_ENV: "/stale/dial"}
        self.assertEqual(host.environment("/fresh/dial", base)[host.HOST_ENV], "/fresh/dial")
        self.assertNotIn(host.HOST_ENV, host.environment(None, base))
        self.assertEqual(base[host.HOST_ENV], "/stale/dial")  # caller's mapping is not mutated

    def test_main_runs_the_command_with_the_verified_host_only(self):
        with tempfile.TemporaryDirectory() as directory:
            exe = str(Path(directory) / "target" / "dev" / "dial")
            run = FakeRun(exe)

            def dispatch(command, **kwargs):
                if command[0] == "test-command":
                    run.calls.append((command, kwargs))
                    return subprocess.CompletedProcess(command, 0)
                return run(command, **kwargs)

            with patch.dict(os.environ, {host.HOST_ENV: "/stale/dial"}), \
                    patch.object(host.subprocess, "run", side_effect=dispatch):
                status = host.main(["--cargo", "cargo", "--", "test-command", "--lib"])
        self.assertEqual(status, 0)
        command, kwargs = run.calls[-1]
        self.assertEqual(command, ["test-command", "--lib"])
        self.assertEqual(kwargs["env"][host.HOST_ENV], exe)

    def test_slim_test_command_gets_no_host_even_if_inherited(self):
        run = FakeRun("/unused")

        def dispatch(command, **kwargs):
            if command[0] == "cargo":
                return run(command, **kwargs)
            run.calls.append((command, kwargs))
            return subprocess.CompletedProcess(command, 0)

        with patch.dict(os.environ, {host.HOST_ENV: "/stale/dial"}), \
                patch.object(host.subprocess, "run", side_effect=dispatch):
            status = host.main(["--no-default-features", "--", "test-command"])
        self.assertEqual(status, 0)
        self.assertEqual(len(run.calls), 1)  # only the test command; no cargo, no smoke check
        self.assertNotIn(host.HOST_ENV, run.calls[0][1]["env"])

    def test_usage_errors_are_explicit(self):
        for argv in ([], ["--profile", "ci"], ["--profile", "ci", "--"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                host.main(argv)


@unittest.skipUnless(shutil.which("make"), "make is required for the Makefile dry-run contract")
class MakefileWiringTests(unittest.TestCase):
    ENV_NAMES = ("MAKEFLAGS", "MFLAGS", "CARGO_TARGET_DIR", "SLIM", "FEATURES", "PROFILE",
                 "TEST_PROFILE", "CRATE", "NEXTEST_PROFILE")

    def dry_run(self, *args):
        env = {name: value for name, value in os.environ.items() if name not in self.ENV_NAMES}
        result = subprocess.run(["make", "-n", "PYTHON=python3", "CARGO=cargo", *args], cwd=ROOT,
                                capture_output=True, text=True, env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout.splitlines()

    def test_every_cargo_test_entry_point_runs_through_the_host_wrapper(self):
        # (make arguments, profile flag the host build must carry or None for cargo's default)
        table = [
            (["test-rust"], "--profile dev"),
            (["test-lib"], None),
            (["test-integration"], None),
            (["test-module", "MOD=llm::provider"], None),
            (["test-nextest"], "--profile dev"),
        ]
        for args, profile in table:
            with self.subTest(target=args[0]):
                lines = self.dry_run(*args)
                wrapped = [line for line in lines if "scripts/embedded-host.py" in line]
                bare = [line for line in lines
                        if re.search(r"\bcargo (test|nextest run)\b", line)
                        and "scripts/embedded-host.py" not in line]
                self.assertTrue(wrapped, lines)
                self.assertEqual(bare, [], "cargo test entry point bypasses the host wrapper")
                for line in wrapped:
                    self.assertRegex(line, r'--cargo "?cargo"? ')
                    self.assertRegex(line, r" -- cargo (test|nextest run) ")
                    if profile:
                        self.assertIn(profile, line.split(" -- ")[0])
                    else:
                        self.assertNotIn("--profile", line.split(" -- ")[0])

    def test_profile_and_feature_flags_reach_the_host_build_unchanged(self):
        line = next(l for l in self.dry_run("test-rust", "TEST_PROFILE=ci")
                    if "scripts/embedded-host.py" in l)
        self.assertIn("--profile ci", line.split(" -- ")[0])
        slim = next(l for l in self.dry_run("test-lib", "SLIM=1", "FEATURES=aws,embedded-shell")
                    if "scripts/embedded-host.py" in l)
        before, _, after = slim.partition(" -- ")
        for part in (before, after):
            self.assertIn("--no-default-features", part)
            self.assertIn('--features "aws,embedded-shell"', part)

    def test_contract_target_is_wired_into_the_ci_test_job(self):
        self.assertTrue(any("scripts/test-embedded-host.py" in line
                            for line in self.dry_run("test-embedded-host")))
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        self.assertIn("run: make test-embedded-host", ci)


if __name__ == "__main__":
    unittest.main()
