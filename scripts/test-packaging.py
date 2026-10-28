#!/usr/bin/env python3
"""Regression tests for scripts/check-packaging.py (run in CI; no Nix or network needed)."""
import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


def load_checker():
    # The checker is a hyphenated script (like scripts/check-artifact.py), so load it by path.
    path = Path(__file__).resolve().with_name("check-packaging.py")
    spec = importlib.util.spec_from_file_location("check_packaging", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_artifact_checker():
    path = Path(__file__).resolve().with_name("check-artifact.py")
    spec = importlib.util.spec_from_file_location("check_artifact", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


artifact = load_artifact_checker()
packaging = load_checker()


GOOD_FLAKE = """
    default = pkgs.rustPlatform.buildRustPackage {
      pname = "dial";
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
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


def REAL_FORMULA():
    return (Path(__file__).resolve().parent.parent / "Formula" / "dial.rb").read_text(encoding="utf-8")


class PackagingTests(unittest.TestCase):
    def test_repository_packaging_is_consistent(self):
        _, problems = packaging.check()
        self.assertEqual(problems, [])

    def test_cargo_version_comes_from_the_workspace_package_table(self):
        cargo = ('[dependencies]\nversion = "9.9.9"\n\n[package]\nname = "dial"\nversion.workspace = true\n\n'
                 '[workspace.package]\nversion = "0.1.8"\n')
        self.assertEqual(packaging.cargo_version(cargo), "0.1.8")

    def test_flake_derived_version_is_accepted(self):
        self.assertEqual(packaging.flake_problems(GOOD_FLAKE), [])

    def test_flake_hard_coded_version_is_rejected(self):
        problems = packaging.flake_problems(GOOD_FLAKE.replace(
            "(builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version",
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


class ReleaseBuildIdentityTests(unittest.TestCase):
    def test_release_build_requires_an_explicit_revision_without_git_metadata(self):
        source = Path(__file__).resolve().parent.parent / "crates/dial-build-info/build.rs"
        with tempfile.TemporaryDirectory(prefix="dial-release-identity-") as directory:
            root = Path(directory)
            executable = root / ("build-info.exe" if os.name == "nt" else "build-info")
            subprocess.run(["rustc", "--edition=2024", str(source), "-o", str(executable)],
                           check=True, capture_output=True, text=True)
            env = os.environ.copy()
            env.update(CARGO_MANIFEST_DIR=str(root), GIT_CEILING_DIRECTORIES=str(root.parent),
                       PROFILE="release", TARGET="x86_64-unknown-linux-gnu")
            env.pop("GIT_DIR", None)
            env.pop("GIT_WORK_TREE", None)
            env.pop("DIAL_BUILD_REVISION", None)
            env.pop("OUT_DIR", None)
            for revision, accepted in [(None, False), ("unknown", False), ("not-a-hash", False),
                                       ("0123456789abcdef0123456789abcdef01234567", True)]:
                with self.subTest(revision=revision):
                    case_env = env.copy()
                    if revision is not None:
                        case_env["DIAL_BUILD_REVISION"] = revision
                    result = subprocess.run([str(executable)], cwd=root, env=case_env,
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, accepted, result.stderr)
                    if accepted:
                        self.assertIn(f"cargo:rustc-env=DIAL_BUILD_REVISION={revision}", result.stdout)
            for profile in ["debug", "local", "profiling", "release"]:
                for target in [root / "target", root / "target" / "aarch64-apple-darwin"]:
                    with self.subTest(profile=profile, target=target):
                        case_env = env.copy()
                        case_env["OUT_DIR"] = str(target / profile / "build" / "dial-build-info-test" / "out")
                        result = subprocess.run([str(executable)], cwd=root, env=case_env,
                                                capture_output=True, text=True)
                        self.assertEqual(result.returncode == 0, profile != "release", result.stderr)
                        if profile != "release":
                            self.assertIn(f"cargo:rustc-env=DIAL_BUILD_PROFILE={profile}", result.stdout)



class FormulaUpdateTests(unittest.TestCase):
    def load_updater(self):
        path = Path(__file__).resolve().with_name("update-formula.py")
        spec = importlib.util.spec_from_file_location("update_formula", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def sums_for(self, version, digests):
        targets = ["aarch64-apple-darwin", "x86_64-apple-darwin",
                   "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
        return {f"dial-v{version}-{target}.tar.gz": digests[target] for target in targets}

    def test_generated_formula_covers_every_homebrew_target_and_stays_consistent(self):
        updater = self.load_updater()
        digests = {t: f"{i:064x}" for i, t in enumerate(
            ["aarch64-apple-darwin", "x86_64-apple-darwin",
             "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"], start=1)}
        formula = updater.update_formula(REAL_FORMULA(), "0.2.0", self.sums_for("0.2.0", digests))
        self.assertEqual(packaging.formula_problems(formula), [])
        self.assertIn('  version "0.2.0"', formula)
        for target, digest in digests.items():
            self.assertIn(f'dial-v#{{version}}-{target}.tar.gz"\n      sha256 "{digest}"', formula)

    def test_missing_homebrew_checksum_aborts_without_rendering(self):
        updater = self.load_updater()
        sums = self.sums_for("0.2.0", {t: "0" * 64 for t in [
            "aarch64-apple-darwin", "x86_64-apple-darwin", "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu"]})
        del sums["dial-v0.2.0-aarch64-unknown-linux-gnu.tar.gz"]
        with self.assertRaises(SystemExit):
            updater.update_formula(REAL_FORMULA(), "0.2.0", sums)


class ArtifactFormatTests(unittest.TestCase):
    @staticmethod
    def elf(machine=0x3E, elf_class=2, endian=1, interpreter=b"/lib64/ld-linux-x86-64.so.2\0"):
        header = bytearray(64)
        header[0:6] = bytes([0x7F, ord("E"), ord("L"), ord("F"), elf_class, endian])
        header[18:20] = machine.to_bytes(2, "little")
        header[32:40] = (64).to_bytes(8, "little")      # e_phoff
        header[54:56] = (56).to_bytes(2, "little")      # e_phentsize
        header[56:58] = (1).to_bytes(2, "little")       # e_phnum
        string_offset = 64 + 56
        phdr = bytearray(56)
        phdr[0:4] = artifact.PT_INTERP.to_bytes(4, "little")
        phdr[8:16] = string_offset.to_bytes(8, "little")
        phdr[32:40] = len(interpreter).to_bytes(8, "little")
        return bytes(header + phdr + interpreter)

    def check(self, data, target):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "dial"
            executable.write_bytes(data)
            artifact.check_target_format(executable, target)

    def test_64_bit_little_endian_glibc_elf_is_accepted(self):
        self.check(self.elf(), "x86_64-unknown-linux-gnu")
        self.check(self.elf(machine=0xB7), "aarch64-unknown-linux-gnu")

    def test_musl_elf_with_musl_loader_is_accepted(self):
        self.check(self.elf(interpreter=b"/lib/ld-musl-x86_64.so.1\0"), "x86_64-unknown-linux-musl")

    def test_big_endian_or_32_bit_elf_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "64-bit little-endian ELF"):
            self.check(self.elf(endian=2), "x86_64-unknown-linux-gnu")
        with self.assertRaisesRegex(RuntimeError, "64-bit little-endian ELF"):
            self.check(self.elf(elf_class=1), "x86_64-unknown-linux-gnu")

    def test_wrong_machine_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "64-bit little-endian ELF"):
            self.check(self.elf(machine=0xB7), "x86_64-unknown-linux-gnu")

    def test_glibc_target_must_link_ld_linux(self):
        with self.assertRaisesRegex(RuntimeError, "does not link ld-linux"):
            self.check(self.elf(interpreter=b"/lib/ld-musl-x86_64.so.1\0"), "x86_64-unknown-linux-gnu")


if __name__ == "__main__":
    unittest.main()
