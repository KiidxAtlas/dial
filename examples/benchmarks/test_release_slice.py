"""Offline contract for release-slice.toml. No model, network or Dial binary is involved."""
import shutil
import tempfile
import tomllib
import unittest
from pathlib import Path

import release_slice

ROOT = Path(__file__).resolve().parent


class ManifestTests(unittest.TestCase):
    def test_committed_slice_matches_its_suites_and_profiles(self):
        manifest = release_slice.load()
        self.assertEqual(manifest["name"], "release-slice-v1")
        self.assertGreaterEqual(manifest["repetitions"], 3)
        cases = sum(suite["cases"] for suite in manifest["suites"])
        self.assertEqual(cases, 27)
        self.assertEqual(release_slice.trials(manifest), 81)

    def test_local_profile_cannot_reach_a_remote_provider(self):
        manifest = release_slice.load()
        local = tomllib.loads((ROOT / manifest["profiles"]["local"]).read_text())
        self.assertFalse(local["machine"]["allow_remote"])
        self.assertEqual({c["provider"] for c in local["configurations"]}, {"lm-studio"})
        self.assertEqual(local["configurations"][0]["model"], release_slice.PLACEHOLDER_MODEL)
        hosted = tomllib.loads((ROOT / manifest["profiles"]["hosted"]).read_text())
        self.assertTrue(hosted["machine"]["allow_remote"])

    def test_plan_covers_every_suite_once_and_writes_under_benchmarks(self):
        manifest = release_slice.load()
        commands = release_slice.plan(manifest, "local", "v9", dial="./dial")
        runs = [c for c in commands if c[:2] == ["./dial", "benchmark"]]
        self.assertEqual([Path(c[2]).name for c in runs], [s["path"] for s in manifest["suites"]])
        for run in runs:
            self.assertEqual(run[4], "examples/benchmarks/release-slice-local-profiles.toml")
            self.assertTrue(run[6].startswith("benchmarks/release-slice/v9/"), run)
        analyses = [c for c in commands if c[1:2] == ["examples/benchmarks/reliability.py"]]
        self.assertEqual([c[2] for c in analyses], [c[6] for c in runs])


class RejectionTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)
        manifest = release_slice.load()
        names = [s["path"] for s in manifest["suites"]] + list(manifest["profiles"].values())
        for name in names + ["release-slice.toml"]:
            shutil.copy(ROOT / name, self.dir / name)
        self.path = self.dir / "release-slice.toml"

    def rewrite(self, name, old, new):
        path = self.dir / name
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def test_copied_slice_is_valid_before_mutation(self):
        release_slice.load(self.path)

    def test_single_attempt_slices_are_refused(self):
        self.rewrite("release-slice.toml", "repetitions = 3", "repetitions = 1")
        with self.assertRaisesRegex(ValueError, "repetitions must be 3"):
            release_slice.load(self.path)

    def test_a_changed_case_count_is_refused(self):
        self.rewrite("release-slice.toml", 'path = "reliability.toml"\ncases = 3', 'path = "reliability.toml"\ncases = 2')
        with self.assertRaisesRegex(ValueError, "reliability.toml has 3 cases"):
            release_slice.load(self.path)

    def test_a_profile_with_other_repetitions_is_refused(self):
        self.rewrite("release-slice-hosted-profiles.toml", "repetitions = 3", "repetitions = 1")
        with self.assertRaisesRegex(ValueError, "differ from the slice"):
            release_slice.load(self.path)

    def test_a_local_profile_that_allows_remote_is_refused(self):
        self.rewrite("release-slice-local-profiles.toml", "allow_remote = false", "allow_remote = true")
        with self.assertRaisesRegex(ValueError, "allow_remote must be false"):
            release_slice.load(self.path)


if __name__ == "__main__":
    unittest.main()
