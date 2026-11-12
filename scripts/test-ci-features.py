#!/usr/bin/env python3
"""Offline tests for CI feature selection; no compilation or network required."""
import importlib.util
import itertools
import json
from pathlib import Path
import subprocess
import sys
import unittest
import tomllib

SCRIPT = Path(__file__).resolve().with_name("ci-features.py")
spec = importlib.util.spec_from_file_location("ci_features", SCRIPT)
features = importlib.util.module_from_spec(spec)
spec.loader.exec_module(features)


def selections(matrix):
    return [frozenset(row["features"].split()) for row in matrix["include"]]


class FeatureMatrixTests(unittest.TestCase):
    def test_push_matrix_covers_boundaries_and_defaults(self):
        selected = selections(features.matrix())
        self.assertEqual(selected, [frozenset(), frozenset(features.FEATURES[:-1]),
                                    frozenset(features.FEATURES)])
        self.assertTrue(set(selected) < set(selections(features.matrix(True))))

    def test_matrix_tracks_the_workspace_feature_contract(self):
        manifest = tomllib.loads((SCRIPT.parent.parent / "Cargo.toml").read_text())
        declared = manifest["features"]
        self.assertEqual(set(features.FEATURES), set(declared) - {"default"})
        self.assertIn(frozenset(declared["default"]), selections(features.matrix()))
        self.assertIn("desktop", declared["wayland-pipewire"])

    def test_exhaustive_matrix_checks_each_of_the_128_combinations_once(self):
        selected = selections(features.matrix(True))
        expected = {
            frozenset(name for name, enabled in zip(features.FEATURES, switches) if enabled)
            for switches in itertools.product([False, True], repeat=7)
        }
        self.assertEqual(len(selected), 128)
        self.assertEqual(set(selected), expected)

    def test_pipewire_uses_newer_runner_and_only_largest_selections_write_caches(self):
        for exhaustive in [False, True]:
            with self.subTest(exhaustive=exhaustive):
                rows = features.matrix(exhaustive)["include"]
                for row in rows:
                    expected_os = "ubuntu-24.04" if "wayland-pipewire" in row["features"].split() else "ubuntu-22.04"
                    self.assertEqual(row["os"], expected_os)
                    self.assertIsInstance(row["save-cache"], bool)
                writers = [row for row in rows if row["save-cache"]]
                self.assertEqual(len(writers), 2)
                for writer in writers:
                    self.assertEqual(len(writer["features"].split()),
                                     max(len(row["features"].split()) for row in rows if row["os"] == writer["os"]))

    def test_cli_emits_one_line_json_for_both_modes(self):
        for args, exhaustive in [([], False), (["--exhaustive"], True)]:
            with self.subTest(args=args):
                result = subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True, check=True)
                self.assertEqual(len(result.stdout.splitlines()), 1)
                self.assertEqual(json.loads(result.stdout), features.matrix(exhaustive))


if __name__ == "__main__":
    unittest.main()
