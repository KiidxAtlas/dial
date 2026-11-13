#!/usr/bin/env python3
"""Offline behavioral regression coverage for release admission and latest selection."""
import importlib.util
import base64
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tarfile
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
        for name in ("release.sh", "release-policy.py", "check-packaging.py", "release-candidate-run.sh"):
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
        log = (self.root / "CHANGELOG.md").read_text()
        self.assertIn("[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.3.0...HEAD", log)
        self.assertEqual(log.count("[0.3.0]:"), 1)
        result = self.release("0.3.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), log)

    def test_new_release_repairs_links_and_preserves_historical_commit_references(self):
        path = self.root / "CHANGELOG.md"
        history = "[0.1.0]: https://github.com/KiidxAtlas/dial/commit/198734a\n"
        path.write_text('## [Unreleased]\n\n- New change.\n\n## [0.3.0]\n\n- Old notes.\n'
                        '\n## [0.1.0]\n\n- Historical notes.\n\n' + history +
                        '[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.2.0...HEAD\n')
        # The Cargo shim does not resolve dependencies; supply the resolver's expected output.
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('"0.3.0"', '"0.4.0"'))
        result = self.release("0.4.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        prepared = path.read_text()
        self.assertIn('version = "0.4.0"', (self.root / "Cargo.toml").read_text())
        self.assertIn("## [0.4.0] - ", prepared)
        self.assertIn("- New change.", policy.release_notes("v0.4.0", prepared))
        self.assertIn(history, prepared)
        self.assertIn("[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.4.0...HEAD", prepared)
        self.assertIn("[0.3.0]: https://github.com/KiidxAtlas/dial/tree/v0.3.0", prepared)
        result = self.release("0.4.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(path.read_text(), prepared)

    def test_invalid_changelog_references_do_not_write_either_source_file(self):
        for source in ('', '## [Unreleased]\n\n[Unreleased]: https://example.com\n',
                       '## [Unreleased]\n- New.\n## [0.3.0]\n- Old.\n'
                       '[0.3.0]: https://example.com/one\n[0.3.0]: https://example.com/two\n',
                       '## [Unreleased]\n- New.\n## [0.3.0]\n- Old.\n[0.3.0]: broken\n'):
            with self.subTest(source=source):
                path = self.root / "CHANGELOG.md"
                path.write_text(source)
                result = self.release("0.4.0")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual((self.root / "Cargo.toml").read_text(), self.manifest)
                self.assertEqual(path.read_text(), source)
                self.assertFalse(any(line.startswith("cargo ") for line in
                                     (self.root / "calls").read_text().splitlines()))

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


class WindowsReleaseTests(unittest.TestCase):
    def test_windows_release_needs_no_azure_but_still_enters_signed_manifest(self):
        workflow = (SCRIPT.parent.parent / ".github/workflows/release.yml").read_text()
        candidates = (SCRIPT.parent.parent / ".github/workflows/release-build.yml").read_text()
        build = candidates.split("\n  windows-build:\n", 1)[1]
        manifest = workflow.split("\n  sign:\n", 1)[1].split("\n  publish:\n", 1)[0]
        self.assertIn("name: release-assets-${{ matrix.target }}", build)
        self.assertIn("Test installer under Windows PowerShell 5.1", build)
        self.assertIn("Test installer under PowerShell 7", build)
        self.assertNotIn("id-token: write", build)
        self.assertNotIn("\n  windows-sign:\n", workflow)
        self.assertNotIn("azure/artifact-signing-action@", workflow)
        self.assertNotIn("DIAL_WINDOWS_", workflow)
        self.assertIn("needs: [gate, signing-ready, build]", manifest)
        self.assertIn("needs.gate.result == 'success'", manifest)
        self.assertIn("needs.signing-ready.result == 'success'", manifest)
        self.assertIn("needs.build.result == 'success' || needs.build.result == 'skipped'", manifest)
        self.assertIn("pattern: release-assets-*", manifest)
        self.assertIn("environment: release-signing", manifest)
        self.assertIn("openssl pkeyutl -sign", manifest)
        self.assertIn("dist/SHA256SUMS", manifest)


class PullRequestCandidateRunTests(unittest.TestCase):
    """Runs scripts/release-candidate-run.sh against a real Git history and a fake `gh`."""

    FAKE_GH = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
state = Path(os.environ["FAKE_GH_STATE"])
with open(state / "calls", "a") as calls:
    calls.write(" ".join(args) + "\\n")
if args[:1] == ["api"]:
    print((state / "pulls.json").read_text())
elif args[:2] == ["run", "list"]:
    print((state / "runs.json").read_text())
elif args[:2] == ["run", "download"]:
    target = Path(args[args.index("-D") + 1])
    target.mkdir(parents=True)
    (target / "candidate.json").write_text((state / "candidate.json").read_text())
else:
    sys.exit(2)
'''

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.repo = self.base / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        for name in ("release-policy.py", "release-candidate-run.sh"):
            shutil.copy2(SCRIPT.with_name(name), self.repo / "scripts" / name)
        self.bin = self.base / "bin"
        self.bin.mkdir()
        (self.bin / "gh").write_text(self.FAKE_GH)
        (self.bin / "gh").chmod(0o755)
        self.git("init", "-q", "-b", "main")
        (self.repo / "file").write_text("one\n")
        self.git("add", ".")
        self.git("commit", "-q", "-m", "base")
        self.git("switch", "-q", "-c", "release")
        (self.repo / "file").write_text("two\n")
        self.git("commit", "-q", "-am", "release")
        self.head = self.git("rev-parse", "HEAD")
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "merge release", "release")
        self.merge = self.git("rev-parse", "HEAD")
        self.tree = self.git("rev-parse", "HEAD^{tree}")

    def git(self, *args):
        env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
        return subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=self.repo, env=env,
                              check=True, capture_output=True, text=True).stdout.strip()

    def find(self, commit, head=None, repo="KiidxAtlas/dial", conclusion="success", tree=None):
        head = head or self.head
        (self.base / "pulls.json").write_text(json.dumps([dict(
            merged_at="2026-11-11T00:00:00Z", base=dict(ref="main"),
            head=dict(sha=head, repo=dict(full_name=repo)))]))
        (self.base / "runs.json").write_text(json.dumps([dict(
            event="pull_request", headSha=head, status="completed", conclusion=conclusion, databaseId=42)]))
        (self.base / "candidate.json").write_text(json.dumps(dict(
            revision=head, tree=tree or self.tree, tag="v0.3.5", public_key="key")))
        return subprocess.run(["bash", "scripts/release-candidate-run.sh", commit], cwd=self.repo,
                              env=dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                                       FAKE_GH_STATE=str(self.base), GITHUB_REPOSITORY="KiidxAtlas/dial",
                                       RELEASE_TAG="v0.3.5", DIAL_RELEASE_PUBLIC_KEY="other"),
                              capture_output=True, text=True)

    def test_merged_same_tree_pull_request_run_is_reused(self):
        result = self.find(self.merge)
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "42"), result.stderr)
        self.assertIn(f"api repos/KiidxAtlas/dial/commits/{self.merge}/pulls",
                      (self.base / "calls").read_text())

    def test_other_trees_forks_failures_and_non_ancestor_heads_are_refused(self):
        squashed = self.git("commit-tree", f"{self.head}^{{tree}}", "-p", f"{self.merge}~1", "-m", "squash")
        main_moved = self.git("commit-tree", f"{self.merge}~1^{{tree}}", "-p", self.merge, "-m", "revert")
        for name, kwargs in (("fork", dict(repo="fork/dial")), ("failed run", dict(conclusion="failure")),
                             ("identity for another tree", dict(tree="f" * 40)),
                             ("tag tree differs from head tree", dict(commit=main_moved)),
                             ("squash merge: head not in history", dict(commit=squashed))):
            with self.subTest(name):
                result = self.find(kwargs.pop("commit", self.merge), **kwargs)
                self.assertEqual((result.returncode, result.stdout), (1, ""), result.stderr)


class CandidatePromotionTests(unittest.TestCase):
    def test_ci_run_selection_never_promotes_pr_failed_or_other_commit(self):
        good = dict(event="push", headBranch="main", headSha="abc", status="completed",
                    conclusion="success", databaseId=123)
        self.assertEqual(policy.successful_ci_run_id([good], "abc"), 123)
        for changes in ({"event": "pull_request"}, {"headBranch": "other"}, {"headSha": "other"},
                        {"status": "in_progress"}, {"conclusion": "failure"},
                        {"databaseId": "123"}, {"databaseId": True}, {"databaseId": 0}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                policy.successful_ci_run_id([{**good, **changes}], "abc")

    def test_all_eight_unexpired_platforms_and_identity_are_required(self):
        artifacts = [dict(name=f"release-assets-{target}", expired=False) for target in policy.RELEASE_TARGETS]
        artifacts.append(dict(name="release-assets-identity", expired=False))
        self.assertTrue(policy.complete_candidate_artifacts([{"artifacts": artifacts[:4]}, {"artifacts": artifacts[4:]}]))
        for invalid in (artifacts[:-1], artifacts[1:], artifacts + [artifacts[0]],
                        [{**a, "expired": True} for a in artifacts],
                        [{"name": a["name"]} for a in artifacts]):
            with self.subTest(invalid=invalid):
                self.assertFalse(policy.complete_candidate_artifacts([{"artifacts": invalid}]))

    def test_identity_cli_refuses_tree_version_and_public_key_changes(self):
        tree, revision = "a" * 40, "b" * 40
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "candidate.json"
            expected = dict(revision=revision, tree=tree, tag="v0.3.0", public_key="public-fixture")
            for changes in ({}, {"tree": "c" * 40}, {"tag": "v0.3.1"}, {"public_key": "changed"},
                            {"revision": "abc"}, {"revision": None}, {"extra": 1}):
                path.write_text(json.dumps({**expected, **changes}))
                result = subprocess.run([sys.executable, str(SCRIPT), "identity", tree, str(path)],
                                        env=dict(os.environ, RELEASE_TAG="v0.3.0", DIAL_RELEASE_PUBLIC_KEY="public-fixture"),
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, not changes, (changes, result.stderr))
            # The tree-only check used to find pull-request candidates ignores the public key,
            # which the release workflow rechecks, but nothing else.
            for changes, accepted in (({"public_key": "changed"}, True), ({"tree": "c" * 40}, False),
                                      ({"tag": "v0.3.1"}, False)):
                path.write_text(json.dumps({**expected, **changes}))
                result = subprocess.run([sys.executable, str(SCRIPT), "candidate-tree", tree, str(path)],
                                        env=dict(os.environ, RELEASE_TAG="v0.3.0", DIAL_RELEASE_PUBLIC_KEY="public-fixture"),
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, accepted, (changes, result.stderr))

    def test_only_same_repository_merged_main_pull_requests_and_their_successful_runs_qualify(self):
        head = "d" * 40
        good = dict(merged_at="2026-11-11T00:00:00Z", base=dict(ref="main"),
                    head=dict(sha=head, repo=dict(full_name="KiidxAtlas/dial")))
        self.assertEqual(policy.merged_pull_request_heads([good], "KiidxAtlas/dial"), [head])
        for changes in ({"merged_at": None}, {"base": dict(ref="release")},
                        {"head": dict(sha=head, repo=dict(full_name="fork/dial"))},
                        {"head": dict(sha=head, repo=None)}, {"head": dict(sha="short", repo=dict(full_name="KiidxAtlas/dial"))}):
            with self.subTest(changes=changes):
                self.assertEqual(policy.merged_pull_request_heads([{**good, **changes}], "KiidxAtlas/dial"), [])
        run = dict(event="pull_request", headSha=head, status="completed", conclusion="success", databaseId=7)
        self.assertEqual(policy.successful_pull_request_ci_run_id([run], head), 7)
        for changes in ({"event": "push"}, {"headSha": "e" * 40}, {"status": "in_progress"},
                        {"conclusion": "failure"}, {"databaseId": "7"}, {"databaseId": True}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                policy.successful_pull_request_ci_run_id([{**run, **changes}], head)

    def test_release_reuses_same_tree_pull_request_ci_or_waits_for_main(self):
        root = SCRIPT.parent.parent
        ci = (root / ".github/workflows/ci.yml").read_text()
        release = (root / ".github/workflows/release.yml").read_text()
        helper = (root / "scripts/release-candidate-run.sh").read_text()
        validate = ci.split("\n  validate:\n", 1)[1].split("\n  release-candidates:\n", 1)[0]
        self.assertIn("fetch-depth: 0", validate)
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", validate)
        # Pull requests record candidates only when the tested merge tree is the built head tree.
        self.assertIn('"$(git rev-parse \'HEAD^{tree}\')" != "$(git rev-parse "$HEAD_SHA^{tree}")"', validate)
        self.assertIn("ref: ${{ needs.validate.outputs.candidate-ref }}", ci)
        self.assertIn('git merge-base --is-ancestor "$head" "$commit"', helper)
        self.assertIn("--event pull_request", helper)
        self.assertIn('candidate-tree "$tree"', helper)
        self.assertIn("if: steps.pr.outputs.run == ''", release)
        self.assertIn("CI_RUN: ${{ steps.pr.outputs.run || steps.ci.outputs.run }}", release)
        self.assertIn("ref: ${{ github.sha }}", release.split("\n  build:\n", 1)[1])

    def test_main_only_candidates_keep_private_keys_out_and_allow_safe_fallback(self):
        root = SCRIPT.parent.parent
        ci = (root / ".github/workflows/ci.yml").read_text()
        candidates = (root / ".github/workflows/release-build.yml").read_text()
        release = (root / ".github/workflows/release.yml").read_text()
        self.assertIn("github.event_name == 'push' && github.ref == 'refs/heads/main'", ci)
        self.assertIn("uses: ./.github/workflows/release-build.yml", ci)
        self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' }}", candidates)
        self.assertNotIn("secrets.", candidates)
        self.assertNotIn("environment: release-signing", candidates)
        self.assertEqual(candidates.count("            target:"), len(policy.RELEASE_TARGETS))
        for target in policy.RELEASE_TARGETS:
            self.assertIn(f"target: {target}", candidates)
        self.assertIn("if: needs.gate.outputs.candidates == ''", release)
        self.assertIn("run-id: ${{ needs.gate.outputs.candidates }}", release)
        self.assertIn('identity "$(git rev-parse \'HEAD^{tree}\')" dist/candidate.json', release)
        publication = (root / '.github/workflows/release-publish.yml').read_text()
        self.assertIn('uses: ./.github/workflows/release-publish.yml', release)
        self.assertIn('file: Dockerfile.release', publication)
        self.assertIn('Smoke-test published container', publication)

    def test_preparation_is_parallel_but_all_latest_writers_are_serialized(self):
        root = SCRIPT.parent.parent / '.github/workflows'
        release = (root / 'release.yml').read_text()
        publication = (root / 'release-publish.yml').read_text()
        self.assertNotIn('\nconcurrency:', release)
        self.assertIn('group: release-publish', publication)
        self.assertIn('queue: max', publication)
        self.assertIn('cancel-in-progress: false', publication)
        for job in ('publish', 'packaging', 'container'):
            self.assertIn(f'\n  {job}:\n', publication)
        self.assertNotIn('needs: sign', publication)
        self.assertNotIn('needs: [publish, gate]', publication)
        self.assertNotIn('DIAL_RELEASE_SIGNING_KEY', publication)
        self.assertIn('release-publish.yml@$GITHUB_REF', publication)
        self.assertNotIn('release.yml@$GITHUB_REF', publication)

    def test_successful_promotion_does_not_skip_publication(self):
        release = (SCRIPT.parent.parent / '.github/workflows/release.yml').read_text()
        publish = release.split('\n  publish:\n', 1)[1]
        self.assertIn('needs: sign', publish)
        self.assertIn("if: ${{ !cancelled() && needs.sign.result == 'success' }}", publish)
        self.assertIn('uses: ./.github/workflows/release-publish.yml', publish)

    def test_candidates_keep_artifact_checks_without_repeating_portable_installer_suite(self):
        root = SCRIPT.parent.parent / '.github/workflows'
        candidates = (root / 'release-build.yml').read_text()
        ci = (root / 'ci.yml').read_text()
        self.assertNotIn('bash scripts/test-install.sh', candidates)
        self.assertNotIn('requirements-ci.lock', candidates.split('\n  windows-build:', 1)[0].split('\n  build:', 1)[1])
        self.assertIn('run: make test-install', ci)
        self.assertIn('python3 scripts/check-artifact.py', candidates)
        self.assertIn('Test installer under Windows PowerShell 5.1', candidates)
        self.assertIn('Test installer under PowerShell 7', candidates)

    def test_linux_lint_reuses_feature_dependencies_without_competing_cache_writers(self):
        workflows = SCRIPT.parent.parent / '.github/workflows'
        ci = (workflows / 'ci.yml').read_text().split('\n  lint:\n', 1)[1].split('\n  musl:\n', 1)[0]
        features = (workflows / 'linux-features.yml').read_text()
        self.assertIn("shared-key: ${{ matrix.os == 'ubuntu-22.04' && 'features-ubuntu-22.04' || '' }}", ci)
        self.assertIn("key: ${{ matrix.os != 'ubuntu-22.04' && format('lint-{0}', matrix.os) || '' }}", ci)
        self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' && matrix.os != 'ubuntu-22.04' }}", ci)
        self.assertIn('shared-key: features-${{ matrix.os }}', features)
        self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' && matrix['save-cache'] }}", features)


class ContainerStagingTests(unittest.TestCase):
    def setUp(self):
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
        from cryptography.hazmat.primitives import serialization
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.key = Ed25519PrivateKey.generate()
        self.public = self.key.public_key().public_bytes(serialization.Encoding.PEM, serialization.PublicFormat.SubjectPublicKeyInfo).decode()
        self.tag = "v0.3.0"
        self.package = f"dial-{self.tag}-x86_64-unknown-linux-gnu"
        self.archive = self.root / f"{self.package}.tar.gz"
        spec = importlib.util.spec_from_file_location("stage_container", SCRIPT.with_name("stage-container.py"))
        self.stage = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.stage)

    def archive_and_sign(self, extra=None, missing=None):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name in sorted(self.stage.FILES - {missing}):
                data = ("#!/bin/sh\necho 'dial 0.3.0'\n" if name == "dial" else name + "\n").encode()
                member = tarfile.TarInfo(f"{self.package}/{name}")
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            if extra is not None:
                archive.addfile(extra, io.BytesIO(b""))
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.sign(f"{digest}  {self.archive.name}\n".encode())

    def sign(self, manifest):
        (self.root / "SHA256SUMS").write_bytes(manifest)
        (self.root / "SHA256SUMS.sig").write_bytes(base64.b64encode(self.key.sign(manifest)))

    def test_known_archive_round_trip_and_cli(self):
        self.archive_and_sign()
        destination = self.root / "context"
        result = subprocess.run([sys.executable, str(SCRIPT.with_name("stage-container.py")), str(self.root), str(destination)],
                                env=dict(os.environ, RELEASE_TAG=self.tag, DIAL_RELEASE_PUBLIC_KEY_PEM=self.public),
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual({p.name for p in destination.iterdir()}, self.stage.FILES)
        self.assertEqual((destination / "dial").read_bytes(), b"#!/bin/sh\necho 'dial 0.3.0'\n")
        self.assertEqual((destination / "dial").stat().st_mode & 0o777, 0o755)
        with self.assertRaises(ValueError):
            self.stage.stage(self.root, destination, self.tag, self.public)

    def test_bundled_node_license_is_staged_and_installed(self):
        # Release archives carry the bundled Node.js runtime's license; staging refused it in 0.3.7.
        self.assertIn("LICENSE.node", self.stage.FILES)
        dockerfile = (SCRIPT.parent.parent / "Dockerfile.release").read_text()
        for name in self.stage.FILES - {"dial", "README.md"}:
            self.assertIn(f"COPY {name} /usr/share/doc/dial/{name}", dockerfile)

    def test_signature_digest_and_duplicate_manifest_entries_fail_closed(self):
        self.archive_and_sign()
        manifest = (self.root / "SHA256SUMS").read_bytes()
        cases = ("signature", "digest", "duplicate")
        for case in cases:
            self.archive_and_sign()
            if case == "signature":
                (self.root / "SHA256SUMS.sig").write_bytes(base64.b64encode(b"x" * 64))
            elif case == "digest":
                self.archive.write_bytes(self.archive.read_bytes() + b"tampered")
            else:
                self.sign(manifest * 2)
            destination = self.root / case
            with self.subTest(case=case), self.assertRaises(ValueError):
                self.stage.stage(self.root, destination, self.tag, self.public)
            self.assertFalse(destination.exists())

    def test_missing_traversal_and_symlink_archive_members_are_refused(self):
        traversal = tarfile.TarInfo("../escape")
        link = tarfile.TarInfo(f"{self.package}/unexpected")
        link.type = tarfile.SYMTYPE
        link.linkname = "../../escape"
        for index, (extra, missing) in enumerate(((None, "LICENSE"), (traversal, None), (link, None))):
            self.archive_and_sign(extra=extra, missing=missing)
            destination = self.root / str(index)
            with self.subTest(index=index), self.assertRaises(ValueError):
                self.stage.stage(self.root, destination, self.tag, self.public)
            self.assertFalse(destination.exists())
        self.assertFalse((self.root.parent / "escape").exists())

    def test_release_image_preserves_runtime_without_a_compiler(self):
        root = SCRIPT.parent.parent
        source = (root / "Dockerfile").read_text().split("FROM debian:", 1)[1]
        release = (root / "Dockerfile.release").read_text().split("FROM debian:", 1)[1]
        # The release archive's binary embeds the Node.js runtime, so its image also carries that license.
        expected = source.replace("COPY --from=build /out/dial", "COPY dial").replace(
            "crates/dial-agent/src/agent/compact/snapcompact/fonts/", "").replace(
            "USER dial\n", "COPY LICENSE.node /usr/share/doc/dial/LICENSE.node\nUSER dial\n", 1)
        self.assertEqual(release, expected)
        self.assertNotIn("cargo", release)


if __name__ == "__main__":
    unittest.main()
