#!/usr/bin/env python3
"""Regression tests for scripts/check-packaging.py (run in CI; no Nix or network needed)."""
import importlib.util
import unittest
from pathlib import Path


def load_checker():
    # The checker is a hyphenated script (like scripts/check-artifact.py), so load it by path.
    path = Path(__file__).resolve().with_name("check-packaging.py")
    spec = importlib.util.spec_from_file_location("check_packaging", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


packaging = load_checker()


GOOD_FLAKE = """
    default = pkgs.rustPlatform.buildRustPackage {
      pname = "dial";
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
      src = self;
    };
"""
GOOD_FORMULA = """
class Dial < Formula
  version "0.1.8"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "685b07e644fa265aa3d088473c561339e29bf4d85ac6b2e913fbe3fe2d34f55c"
    end
  end
end
"""


class PackagingTests(unittest.TestCase):
    def test_repository_packaging_is_consistent(self):
        _, problems = packaging.check()
        self.assertEqual(problems, [])

    def test_cargo_version_comes_from_the_package_table(self):
        cargo = '[dependencies]\nversion = "9.9.9"\n\n[package]\nname = "dial"\nversion = "0.1.8"\n'
        self.assertEqual(packaging.cargo_version(cargo), "0.1.8")

    def test_flake_derived_version_is_accepted(self):
        self.assertEqual(packaging.flake_problems(GOOD_FLAKE), [])

    def test_flake_hard_coded_version_is_rejected(self):
        problems = packaging.flake_problems(GOOD_FLAKE.replace(
            "(builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version",
            '"0.1.7"'))
        self.assertTrue(any("hard-codes" in p for p in problems), problems)
        self.assertTrue(any("must derive" in p for p in problems), problems)

    def test_formula_must_be_versioned_by_its_urls(self):
        self.assertEqual(packaging.formula_problems(GOOD_FORMULA), [])
        broken = GOOD_FORMULA.replace("download/v#{version}/", "download/v0.1.7/")
        self.assertTrue(packaging.formula_problems(broken))

    def test_formula_urls_need_checksums(self):
        broken = GOOD_FORMULA.replace('      sha256 "685b07e644fa265aa3d088473c561339e29bf4d85ac6b2e913fbe3fe2d34f55c"\n', "")
        self.assertTrue(any("sha256" in p for p in packaging.formula_problems(broken)))


if __name__ == "__main__":
    unittest.main()
