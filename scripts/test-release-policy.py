#!/usr/bin/env python3
"""Offline behavioral regression coverage for release admission and latest selection."""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("release-policy.py")
spec = importlib.util.spec_from_file_location("release_policy", SCRIPT)
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class ReleasePolicyTests(unittest.TestCase):
    def test_only_a_completed_successful_main_push_for_exact_revision_is_accepted(self):
        good = dict(event="push", headBranch="main", headSha="abc", status="completed", conclusion="success")
        self.assertTrue(policy.successful_main_ci([good], "abc"))
        for field, value in [("event", "pull_request"), ("headBranch", "feature"),
                             ("headSha", "other"), ("status", "in_progress"),
                             ("conclusion", "failure")]:
            with self.subTest(field=field):
                self.assertFalse(policy.successful_main_ci([{**good, field: value}], "abc"))
        self.assertFalse(policy.successful_main_ci([], "abc"))
        self.assertFalse(policy.successful_main_ci([{}], "abc"))

    def test_latest_does_not_move_backwards_and_compares_numeric_versions(self):
        self.assertTrue(policy.is_latest("v0.2.10", ["v0.2.9"]))
        self.assertFalse(policy.is_latest("v0.2.9", ["v0.2.10"]))
        self.assertTrue(policy.is_latest("v0.2.10", ["v0.2.10"]))
        self.assertTrue(policy.is_latest("v0.2.10", ["nightly", "v0.3.0-rc.1"]))
        self.assertFalse(policy.is_latest("v0.2.10", ["v1.0.0"]))
        for tag in ["v0.2.10-rc.1", "v01.2.3", "0.2.10", "v0.2.10\nmalicious"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                policy.is_latest(tag, [])

    def test_command_line_refuses_untrusted_ci_and_emits_boolean_latest(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "input"
            source.write_text(json.dumps([dict(event="pull_request", headBranch="main", headSha="abc",
                                              status="completed", conclusion="success")]))
            result = subprocess.run([sys.executable, str(SCRIPT), "ci", "abc", str(source)], capture_output=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            source.write_text("v0.2.10\n")
            result = subprocess.run([sys.executable, str(SCRIPT), "latest", "v0.2.9", str(source)],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "latest=false\n")

    def test_notes_select_only_the_exact_curated_version(self):
        changelog = "# Changelog\n\n## [Unreleased]\n\nFuture.\n\n## [0.2.10] - 2026-11-01\n\n### Fixed\n\n- Tagged change.\n\n## [0.2.9] - 2026-10-31\n\nOlder.\n"
        self.assertEqual(policy.release_notes("v0.2.10", changelog), "### Fixed\n\n- Tagged change.")
        self.assertEqual(policy.release_notes("v0.2.9", changelog), "Older.")
        for tag, source in [
            ("v0.2.8", changelog),
            ("v0.2.10", changelog + "\n## [0.2.10]\nDuplicate.\n"),
            ("v0.2.10", "## [0.2.10]\n\n## [0.2.9]\nOlder."),
            ("v0.2.10-rc.1", changelog),
        ]:
            with self.subTest(tag=tag, source=source), self.assertRaises(ValueError):
                policy.release_notes(tag, source)

    def test_notes_command_line_fails_closed_and_outputs_curated_text(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "CHANGELOG.md"
            source.write_text("## [Unreleased]\nFuture.\n## [0.2.10]\nTagged notes.\n")
            command = [sys.executable, str(SCRIPT), "notes", "v0.2.10", str(source)]
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "Tagged notes.\n")
            source.write_text("## [Unreleased]\nFuture only.\n")
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
