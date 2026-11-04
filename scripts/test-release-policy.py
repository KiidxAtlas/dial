#!/usr/bin/env python3
"""Offline behavioral regression coverage for release admission and latest selection."""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
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


class ReleaseReadinessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = ('[package]\nname = "dial"\nversion.workspace = true\n'
                         '[workspace]\nmembers = [".", "crates/*"]\n'
                         '[workspace.package]\nversion = "0.3.0"\n')
        (self.root / "Cargo.toml").write_text(self.manifest)
        member = self.root / "crates/helper"
        member.mkdir(parents=True)
        (member / "Cargo.toml").write_text('[package]\nname = "helper"\nversion.workspace = true\n')
        (self.root / "Cargo.lock").write_text(
            'version = 4\n[[package]]\nname = "dial"\nversion = "0.3.0"\n'
            '[[package]]\nname = "helper"\nversion = "0.3.0"\n')
        (self.root / "CHANGELOG.md").write_text('## [Unreleased]\n\n## [0.3.0]\n\n- Notes.\n')

    def test_static_gate_accepts_inherited_workspace_without_invoking_compilers(self):
        with mock.patch("subprocess.run", side_effect=AssertionError("no child commands")):
            self.assertEqual(policy.release_problems(self.root, "v0.3.0"), [])

    def test_workspace_glob_ignores_files_but_refuses_directories_without_manifests(self):
        (self.root / "crates/.DS_Store").write_bytes(b"Finder metadata")
        (self.root / "crates/README.md").write_text("Not a workspace package")
        self.assertEqual(policy.release_problems(self.root, "v0.3.0"), [])
        (self.root / "crates/broken").mkdir()
        problems = policy.release_problems(self.root, "v0.3.0")
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("crates/broken/Cargo.toml", problems[0])

    def test_static_gate_aggregates_independent_errors(self):
        (self.root / "crates/helper/Cargo.toml").write_text('[package]\nname = "helper"\nversion = "0.2.0"\n')
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "dial"\nversion = "0.2.0"\n')
        (self.root / "CHANGELOG.md").write_text('## [Unreleased]\nFuture.\n')
        problems = policy.release_problems(self.root, "v0.4.0")
        self.assertEqual(len(problems), 5, problems)
        self.assertTrue(any("must inherit" in p for p in problems))
        self.assertTrue(any("CHANGELOG" in p for p in problems))

    def test_invalid_manifest_missing_member_and_duplicate_notes_are_refused(self):
        for path, content in [
            ("Cargo.toml", "not toml"),
            ("crates/helper/Cargo.toml", ""),
            ("CHANGELOG.md", "## [0.3.0]\nA.\n## [0.3.0]\nB.\n"),
        ]:
            with self.subTest(path=path):
                target = self.root / path
                original = target.read_text()
                target.write_text(content)
                self.assertTrue(policy.release_problems(self.root))
                target.write_text(original)

    def test_tools_reports_every_missing_prerequisite_without_running_it(self):
        with mock.patch("shutil.which", return_value=None), mock.patch("importlib.util.find_spec", return_value=None):
            self.assertEqual(len(policy.tool_problems()), 7)

    def release(self, *args, env_updates=None):
        scripts = self.root / "scripts"
        scripts.mkdir(exist_ok=True)
        for name in ("release.sh", "release-policy.py"):
            shutil.copy2(SCRIPT.with_name(name), scripts / name)
        tools = self.root / "bin"
        tools.mkdir(exist_ok=True)
        # Real Git, Cargo and remote services must never be used by these fixtures.
        for name, body in {
            "git": 'echo "git $*" >> "$CALLS"\ncase "$*" in\n'
                   '"rev-parse --show-toplevel") pwd;;\n'
                   '"rev-parse --abbrev-ref HEAD") echo main;;\n'
                   '"status --porcelain") printf "%s" "${DIRTY:-}";;\n'
                   '"rev-parse -q --verify refs/tags/v0.3.0") exit 1;;\n'
                   '"fetch -q origin main") exit 0;;\n'
                   '"rev-parse HEAD"|"rev-parse origin/main") echo abc;;\n'
                   '"tag -a v0.3.0 -m Dial v0.3.0") exit 0;;\n'
                   '*) exit 91;;\nesac\n',
            "cargo": 'echo "cargo $*" >> "$CALLS"\n[ "$*" = "update --workspace --offline" ]\n',
            "make": 'echo "make $*" >> "$CALLS"\npython3 scripts/release-policy.py check v0.3.0\n',
            "gh": 'echo "gh $*" >> "$CALLS"\ncase "$*" in\n'
                  '"run list --workflow ci.yml --commit abc --event push --branch main --json status,conclusion,event,headBranch,headSha")\n'
                  'printf "%s" "$CI_RUNS";;\n*) exit 0;;\nesac\n',
        }.items():
            path = tools / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(0o755)
        return subprocess.run(["bash", str(scripts / "release.sh"), *args], cwd=self.root,
                              env=dict(os.environ, PATH=f"{tools}{os.pathsep}{os.environ['PATH']}",
                                       CALLS=str(self.root / "calls"), **(env_updates or {})), capture_output=True, text=True)

    def test_prepare_is_repeatable_and_never_commits_tags_pushes_or_compiles(self):
        for _ in range(2):
            result = self.release("0.3.0")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.root / "calls").read_text().splitlines(),
                         ["git rev-parse --show-toplevel", "cargo update --workspace --offline"] * 2)
        self.assertEqual((self.root / "Cargo.toml").read_text(), self.manifest)

    def test_prepare_refusal_does_not_partially_bump_manifest(self):
        result = self.release("0.4.0")  # Unreleased is empty.
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.root / "Cargo.toml").read_text(), self.manifest)
        self.assertEqual((self.root / "calls").read_text().splitlines(), ["git rev-parse --show-toplevel"])

    def test_push_without_explicit_tag_and_legacy_bypass_are_refused(self):
        for flag in ("--push", "--no-check"):
            with self.subTest(flag=flag):
                result = self.release("0.3.0", flag)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "calls").exists())

    def test_tag_requires_clean_prepared_source_and_successful_exact_commit_ci(self):
        good = dict(event="push", headBranch="main", headSha="abc", status="completed", conclusion="success")
        for changes in ({"headSha": "other"}, {"event": "pull_request"}, {"conclusion": "failure"}, {}):
            with self.subTest(changes=changes):
                (self.root / "calls").unlink(missing_ok=True)
                result = self.release("0.3.0", "--tag", env_updates={"CI_RUNS": json.dumps([{**good, **changes}])})
                self.assertEqual(result.returncode == 0, not changes, result.stdout + result.stderr)
                calls = (self.root / "calls").read_text().splitlines()
                self.assertEqual(any(c.startswith("git tag -a") for c in calls), not changes)
                self.assertFalse(any(c.startswith(("git commit", "git push", "cargo ")) for c in calls))
        result = self.release("0.3.0", "--tag", env_updates={"DIRTY": " M Cargo.toml"})
        self.assertNotEqual(result.returncode, 0)


class SigningReadinessTests(unittest.TestCase):
    def test_matching_malformed_wrong_type_and_mismatched_keys(self):
        from base64 import b64encode
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric import ed25519, ec
        key = ed25519.Ed25519PrivateKey.generate()
        other = ed25519.Ed25519PrivateKey.generate()
        def private_pem(value):
            return value.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                       serialization.NoEncryption()).decode()
        def public_pem(value):
            return value.public_key().public_bytes(serialization.Encoding.PEM,
                                                   serialization.PublicFormat.SubjectPublicKeyInfo).decode()
        good = dict(DIAL_RELEASE_PUBLIC_KEY=b64encode(key.public_key().public_bytes_raw()).decode(),
                    DIAL_RELEASE_PUBLIC_KEY_PEM=public_pem(key), DIAL_RELEASE_SIGNING_KEY=private_pem(key))
        cases = [({}, 0), ({"DIAL_RELEASE_PUBLIC_KEY": "invalid"}, 3),
                 ({"DIAL_RELEASE_PUBLIC_KEY_PEM": "missing"}, 1),
                 ({"DIAL_RELEASE_PUBLIC_KEY_PEM": public_pem(other)}, 1),
                 ({"DIAL_RELEASE_SIGNING_KEY": private_pem(other)}, 1),
                 ({"DIAL_RELEASE_SIGNING_KEY": ""}, 1),
                 ({"DIAL_RELEASE_SIGNING_KEY": private_pem(ec.generate_private_key(ec.SECP256R1()))}, 1)]
        for changes, count in cases:
            with self.subTest(fields=list(changes)), mock.patch.dict(os.environ, {**good, **changes}, clear=True):
                problems = policy.key_problems(require_private=True)
                self.assertEqual(len(problems), count, problems)
                self.assertFalse(any("BEGIN PRIVATE KEY" in p for p in problems))
        with mock.patch.dict(os.environ, good, clear=True):
            os.environ.pop("DIAL_RELEASE_SIGNING_KEY")
            self.assertEqual(policy.key_problems(), [])


if __name__ == "__main__":
    unittest.main()
