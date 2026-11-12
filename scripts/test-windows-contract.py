#!/usr/bin/env python3
"""Focused runner regressions; no Cargo build or Windows host required."""
import contextlib
import importlib.util
import io
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "windows_contract", Path(__file__).with_name("windows-contract.py"))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def summary(passed=0, ignored=0):
    return (f"test result: ok. {passed} passed; 0 failed; {ignored} ignored; "
            "0 measured; 20 filtered out; finished in 0.00s\n")


class WindowsContractTests(unittest.TestCase):
    def parse(self, text):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "contract.txt"
            path.write_text(text, encoding="utf-8")
            return contract.parse(path)

    def run_fixture(self, output, code=0, tests=None, skips=None, dry_run=False):
        if tests is None:
            tests = [(["--workspace", "--lib"], "platform::")]
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(contract.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], code, stdout=output)) as run:
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                result = contract.run_contract(tests, skips or [], "ci", dry_run)
        return result, run, stdout.getvalue(), stderr.getvalue()

    def test_repository_update_filter_selects_the_binary_module(self):
        tests, skips = contract.parse(contract.CONTRACT)
        update = [test for test in tests if "update" in test[1]]
        self.assertEqual(update, [(["--package", "dial", "--bin", "dial"],
                                   "cmd::update::tests::")])
        result, run, _, _ = self.run_fixture(summary(3), tests=update, skips=skips)
        self.assertEqual(result, 0)
        command = run.call_args.args[0]
        self.assertEqual(command[:11], ["cargo", "test", "--locked", "--profile", "ci",
                                       "--package", "dial", "--bin", "dial",
                                       "cmd::update::tests::", "--"])
        self.assertNotIn("--lib", command)

    def test_package_library_selects_only_its_owner(self):
        tests, _ = self.parse("test-lib dial-llm config::paths::tests::\n")
        self.assertEqual(tests, [(["--package", "dial-llm", "--lib"], "config::paths::tests::")])
        result, run, _, _ = self.run_fixture(summary(7), tests=tests)
        self.assertEqual(result, 0)
        self.assertEqual(run.call_args.args[0], ["cargo", "test", "--locked", "--profile", "ci",
                                              "--package", "dial-llm", "--lib",
                                              "config::paths::tests::", "--"])

    def test_repository_filters_never_build_all_workspace_test_harnesses(self):
        tests, _ = contract.parse(contract.CONTRACT)
        for target, name in tests:
            with self.subTest(filter=name):
                self.assertNotIn("--workspace", target)
                self.assertEqual(target[0], "--package")

    def test_skips_are_reported_and_applied_before_counting(self):
        tests, skips = self.parse("test platform::\nskip platform::unix # POSIX only\n")
        result, run, output, _ = self.run_fixture(summary(1), tests=tests, skips=skips)
        self.assertEqual(result, 0)
        self.assertIn("SKIPPED platform::unix: POSIX only", output)
        self.assertEqual(run.call_args.args[0][-2:], ["--skip", "platform::unix"])

    def test_no_execution_fails_closed(self):
        # The same empty summary covers a stale filter, platform-cfg exclusion, or all skipped.
        for output in (summary(), summary(ignored=2), "", "some_test: test\n1 test, 0 benchmarks\n"):
            with self.subTest(output=output):
                result, _, _, error = self.run_fixture(output)
                self.assertEqual(result, 1)
                self.assertIn("executed no tests", error)

    def test_workspace_empty_harnesses_do_not_mask_real_execution(self):
        result, _, output, _ = self.run_fixture(summary() + summary(2) + summary())
        self.assertEqual(result, 0)
        self.assertIn("VERIFIED platform::: 2 tests passed", output)

    def test_each_filter_must_execute_tests(self):
        tests = [(["--workspace", "--lib"], name) for name in ("platform::", "missing::")]
        with patch.object(contract.subprocess, "run", side_effect=[
                subprocess.CompletedProcess([], 0, stdout=summary(1)),
                subprocess.CompletedProcess([], 0, stdout=summary())]):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(contract.run_contract(tests, [], "ci"), 1)

    def test_cargo_failure_is_preserved_and_later_filters_still_run(self):
        tests = [(["--workspace", "--lib"], name) for name in ("broken::", "platform::")]
        result, run, output, error = self.run_fixture("compile failed\n", code=101, tests=tests)
        self.assertEqual(result, 101)
        self.assertEqual(run.call_count, 2)
        self.assertIn("compile failed", output)
        self.assertIn("Windows contract failed in filter `broken::`", error)
        self.assertIn("Windows contract failures: `broken::`, `platform::`", error)

    def test_print_is_explicitly_unverified_and_does_not_run_cargo(self):
        result, run, output, _ = self.run_fixture("", dry_run=True)
        self.assertEqual(result, 0)
        run.assert_not_called()
        self.assertIn("--workspace --lib platform::", output)
        self.assertNotIn("VERIFIED", output)

    def test_invalid_or_empty_contracts_are_rejected(self):
        for text in ("", "skip x # reason", "test-bin dial dial", "test-bin dial dial x extra",
                     "test-bin --workspace dial x", "test x extra", "skip x", "unknown x",
                     "test-lib dial-llm", "test-lib dial-llm x extra", "test-lib --workspace x",
                     "test-integration dial integration", "test-integration dial --all x",
                     "test-integration dial integration x extra"):
            with self.subTest(text=text), self.assertRaises(SystemExit):
                self.parse(text)

    def test_integration_target_selects_only_that_test_target(self):
        tests, _ = self.parse("test-integration dial integration isolation::\n")
        self.assertEqual(tests, [(["--package", "dial", "--test", "integration"], "isolation::")])
        result, run, _, _ = self.run_fixture(summary(4), tests=tests)
        self.assertEqual(result, 0)
        self.assertEqual(run.call_args.args[0][:10], ["cargo", "test", "--locked", "--profile", "ci",
                                                     "--package", "dial", "--test", "integration",
                                                     "isolation::"])

    def test_label_names_the_platform_in_failures(self):
        stderr = io.StringIO()
        with patch.object(contract.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], 0, stdout=summary(0))):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                result = contract.run_contract([(["--workspace", "--lib"], "x::")], [], "ci",
                                               label="macOS")
        self.assertEqual(result, 1)
        self.assertIn("macOS contract filter `x::` executed no tests", stderr.getvalue())

    def test_macos_contract_never_builds_every_workspace_harness_and_ci_uses_it(self):
        root = Path(contract.__file__).resolve().parent.parent
        tests, skips = contract.parse(root / "scripts" / "macos-contract.txt")
        self.assertEqual(skips, [])
        self.assertTrue(any(target[:2] == ["--package", "dial-core"] for target, _ in tests))
        self.assertTrue(any("--test" in target for target, _ in tests))
        for target, _ in tests:
            self.assertNotIn("--workspace", target)
        ci = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        job = ci.split("\n  test:\n", 1)[1].split("\n  lint:\n", 1)[0]
        self.assertIn("--contract scripts/macos-contract.txt --label macOS", job)
        for step in ("Build", "Install cargo-nextest (verified archive)", "Test"):
            body = job.split(f"- name: {step}\n", 1)[1].split("\n      - ", 1)[0]
            self.assertIn("if: runner.os == 'Linux'", body, step)


if __name__ == "__main__":
    unittest.main()
