#!/usr/bin/env python3
"""Focused runner regressions; no Cargo build or Windows host required."""
import contextlib
import os
import sys
import importlib.util
import io
import json
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

    def run_fixture(self, output, code=0, tests=None, skips=None, dry_run=False, host=None):
        if tests is None:
            tests = [(["--workspace", "--lib"], "platform::")]
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(contract.subprocess, "run", return_value=subprocess.CompletedProcess(
                [], code, stdout=output)) as run:
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                result = contract.run_contract(tests, skips or [], "ci", dry_run, host=host)
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

    def test_every_filter_gets_the_verified_host_and_never_a_stale_one(self):
        tests = [(["--workspace", "--lib"], "platform::"), (["--package", "dial", "--bin", "dial"], "cmd::")]
        for host, expected in (("C:\\target\\ci\\dial.exe", "C:\\target\\ci\\dial.exe"), (None, None)):
            with self.subTest(host=host), patch.dict(os.environ, {"DIAL_EMBEDDED_SHELL_HOST": "C:\\stale.exe"}):
                result, run, _, _ = self.run_fixture(summary(1), tests=tests, host=host)
                self.assertEqual(result, 0)
                # One prebuild per distinct target selection, then one run per filter; all get the host.
                self.assertEqual(run.call_count, 4)
                for call in run.call_args_list:
                    self.assertEqual(call.kwargs["env"].get("DIAL_EMBEDDED_SHELL_HOST"), expected)

    def test_main_builds_the_host_once_before_filters_and_passes_it_on(self):
        with tempfile.TemporaryDirectory() as directory:
            contract_path = Path(directory) / "contract.txt"
            contract_path.write_text("test-lib dial-core platform::\n", encoding="utf-8")
            stdout = io.StringIO()
            with patch.object(contract.embedded_host, "prepare", return_value="/built/dial") as prepare, \
                    patch.object(contract.subprocess, "run", return_value=subprocess.CompletedProcess(
                        [], 0, stdout=summary(1))) as run, \
                    patch.object(sys, "argv", ["windows-contract.py", "--contract", str(contract_path)]), \
                    contextlib.redirect_stdout(stdout):
                self.assertEqual(contract.main(), 0)
        prepare.assert_called_once_with("cargo", "ci", dry_run=False)
        self.assertEqual(run.call_count, 2)  # the target prebuild, then the filter
        self.assertEqual(run.call_args.kwargs["env"]["DIAL_EMBEDDED_SHELL_HOST"], "/built/dial")

    def test_print_shows_host_preparation_without_building(self):
        with patch.object(contract.embedded_host.subprocess, "run") as host_run, \
                patch.object(sys, "argv", ["windows-contract.py", "--print"]), \
                contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(contract.main(), 0)
        host_run.assert_not_called()
        self.assertIn("+ cargo build --locked -p dial --bin dial", stdout.getvalue())

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
                subprocess.CompletedProcess([], 0, stdout=""),
                subprocess.CompletedProcess([], 0, stdout=summary(1)),
                subprocess.CompletedProcess([], 0, stdout=summary())]):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(contract.run_contract(tests, [], "ci"), 1)

    def test_cargo_failure_is_preserved_and_later_filters_still_run(self):
        tests = [(["--workspace", "--lib"], name) for name in ("broken::", "platform::")]
        result, run, output, error = self.run_fixture("compile failed\n", code=101, tests=tests)
        self.assertEqual(result, 101)
        self.assertEqual(run.call_count, 3)
        self.assertIn("contract prebuild failed", error)
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
        macos = ci.split("\n  test:\n", 1)[1].split("\n  linux-tests:\n", 1)[0]
        self.assertIn("os: [macos-15]", macos)
        self.assertIn("--contract scripts/macos-contract.txt --label macOS", macos)
        for step in ("- name: Build\n", "cargo-nextest", "make test-nextest"):
            self.assertNotIn(step, macos)

    def test_linux_gate_keeps_the_required_check_and_needs_every_linux_job(self):
        root = Path(contract.__file__).resolve().parent.parent
        ci = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        tests = ci.split("\n  linux-tests:\n", 1)[1].split("\n  linux-runtime:\n", 1)[0]
        self.assertIn("partition: [1, 2]", tests)
        self.assertIn("NEXTEST_PARTITION=hash:${{ matrix.partition }}/2", tests)
        self.assertIn("NEXTEST_DOCTESTS=${{ matrix.partition == 2 && '1' || '0' }}", tests)
        gate = ci.split("\n  linux-gate:\n", 1)[1].split("\n  lint:\n", 1)[0]
        self.assertIn("name: fmt, build, test (ubuntu-22.04)\n", gate)
        self.assertIn("needs: [linux-tests, linux-runtime]", gate)
        self.assertIn("if: always()", gate)
        self.assertIn('test "$TESTS" = success && test "$RUNTIME" = success', gate)

    def test_prebuild_compiles_exactly_the_filter_targets_together(self):
        tests, _ = self.parse("test-lib dial-core platform::\n"
                              "test-lib dial-tools tools::\n"
                              "test-lib dial-core more::\n"
                              "test-bin dial dial cmd::\n"
                              "test-integration dial integration isolation::\n")
        build = ["cargo", "test", "--locked", "--profile", "ci", "--no-run",
                 "--message-format=json-render-diagnostics"]
        self.assertEqual(contract.prebuild_commands(tests, "ci"), [
            [*build, "--package", "dial-core", "--package", "dial-tools", "--lib"],
            [*build, "--package", "dial", "--bin", "dial"],
            [*build, "--package", "dial", "--test", "integration"],
        ])
        result, run, _, _ = self.run_fixture(summary(2), tests=tests)
        self.assertEqual(result, 0)
        self.assertEqual(run.call_count, 3 + len(tests))
        self.assertIn("--no-run", run.call_args_list[0].args[0])
        self.assertIn("--message-format=json-render-diagnostics", run.call_args_list[0].args[0])
        self.assertNotIn("--no-run", run.call_args_list[3].args[0])

    def test_filters_run_prebuilt_binaries_so_cargo_never_rebuilds(self):
        def artifact(kind, name, exe, manifest="/repo/crates/x/Cargo.toml", test=True):
            return json.dumps({"reason": "compiler-artifact", "executable": exe,
                               "manifest_path": manifest, "profile": {"test": test},
                               "target": {"kind": [kind], "name": name}}) + "\n"
        lib_build = (artifact("lib", "dial_core", "/t/dial_core-1", "/repo/crates/dial-core/Cargo.toml")
                     + artifact("lib", "dial_core", "/t/not-a-test", test=False)
                     + "not json\n")
        bin_build = artifact("bin", "dial", "/t/dial-2", "/repo/Cargo.toml")
        tests, skips = self.parse("test-lib dial-core platform::\n"
                                  "test-bin dial dial cmd::\n"
                                  "test-lib dial-llm missing::\n"
                                  "skip platform::unix # POSIX only\n")
        with patch.object(contract.subprocess, "run", side_effect=[
                subprocess.CompletedProcess([], 0, stdout=lib_build, stderr=""),
                subprocess.CompletedProcess([], 0, stdout=bin_build, stderr=""),
                subprocess.CompletedProcess([], 0, stdout=summary(4)),
                subprocess.CompletedProcess([], 0, stdout=summary(3)),
                subprocess.CompletedProcess([], 0, stdout=summary(1))]) as run:
            with contextlib.redirect_stdout(io.StringIO()) as out, \
                    contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(contract.run_contract(tests, skips, "ci"), 0)
        calls = [call.args[0] for call in run.call_args_list]
        self.assertEqual(calls[2], ["/t/dial_core-1", "platform::", "--skip", "platform::unix"])
        self.assertEqual(run.call_args_list[2].kwargs["cwd"], "/repo/crates/dial-core")
        self.assertEqual(calls[3], ["/t/dial-2", "cmd::", "--skip", "platform::unix"])
        # No built executable for this target: Cargo still runs it rather than skipping coverage.
        self.assertEqual(calls[4][:2], ["cargo", "test"])
        self.assertIn("VERIFIED platform::: 4 tests passed", out.getvalue())

    def test_a_failed_prebuild_falls_back_to_cargo_for_every_filter(self):
        tests, _ = self.parse("test-lib dial-core platform::\n")
        result, run, _, error = self.run_fixture("error\n", code=101, tests=tests)
        self.assertEqual(result, 101)
        self.assertIn("prebuild failed", error)
        self.assertEqual(run.call_args_list[1].args[0][:2], ["cargo", "test"])


if __name__ == "__main__":
    unittest.main()
