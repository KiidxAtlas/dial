#!/usr/bin/env python3
"""Regression tests for scripts/check-packaging.py (run in CI; no Nix or network needed)."""
import importlib.util
import json
import os
import sys
import re
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



class ChangelogReferenceTests(unittest.TestCase):
    LOG = "## [Unreleased]\n\n## [0.3.0] - 2026-11-05\n\n- Notes.\n\n## [0.1.0]\n\n- History.\n"
    HISTORY = "[0.1.0]: https://github.com/KiidxAtlas/dial/commit/198734a\n"

    def test_missing_and_stale_links_are_repaired_without_rewriting_history(self):
        for refs in ("", self.HISTORY,
                     self.HISTORY + "[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.2.0...HEAD\n"):
            with self.subTest(refs=refs):
                source = self.LOG + refs
                self.assertTrue(packaging.changelog_problems(source))
                repaired = packaging.update_changelog_references(source)
                self.assertEqual(packaging.changelog_problems(repaired), [])
                self.assertIn("[Unreleased]: https://github.com/KiidxAtlas/dial/compare/v0.3.0...HEAD", repaired)
                self.assertIn("[0.3.0]: https://github.com/KiidxAtlas/dial/tree/v0.3.0", repaired)
                if refs:
                    self.assertIn(self.HISTORY, repaired)
                self.assertEqual(packaging.update_changelog_references(repaired), repaired)

    def test_ambiguous_or_malformed_changelogs_fail_closed(self):
        valid = packaging.update_changelog_references(self.LOG + self.HISTORY)
        for source in ("", "## [Unreleased]\n", "## [0.3.0]\nNotes.\n",
                       self.LOG.replace("[0.3.0]", "[not-a-version]"),
                       self.LOG + "## [0.3.0]\nDuplicate.\n",
                       valid + "[0.3.0]: https://example.com/duplicate\n",
                       valid + "[unreleased]: https://example.com/duplicate\n",
                       valid.replace("/tree/v0.3.0", "/tree/v0.2.0"),
                       valid.replace("/commit/198734a", "/commit/not-a-revision"),
                       valid.replace("[0.3.0]: https://github.com/KiidxAtlas/dial/tree/v0.3.0", "[0.3.0]:")):
            with self.subTest(source=source):
                self.assertTrue(packaging.changelog_problems(source))
                with self.assertRaises(ValueError):
                    packaging.update_changelog_references(source)

    def test_packaging_check_includes_reference_drift(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Formula").mkdir()
            (root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.3.0"\n')
            (root / "flake.nix").write_text(GOOD_FLAKE)
            (root / "Formula/dial.rb").write_text(GOOD_FORMULA)
            (root / "CHANGELOG.md").write_text(self.LOG)
            _, problems = packaging.check(root)
            self.assertTrue(problems)
            self.assertTrue(all("CHANGELOG.md" in p for p in problems))

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
            env.pop("SOURCE_DATE_EPOCH", None)
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
                        self.assertIn("cargo:rustc-env=DIAL_BUILD_EPOCH=0\n", result.stdout)
                        repeat = subprocess.run([str(executable)], cwd=root, env=case_env,
                                                capture_output=True, text=True, check=True)
                        self.assertEqual(repeat.stdout, result.stdout)
                        for epoch, valid in [("1234567890", True), ("not-an-epoch", False)]:
                            with self.subTest(epoch=epoch):
                                stamped = subprocess.run([str(executable)], cwd=root,
                                    env={**case_env, "SOURCE_DATE_EPOCH": epoch}, capture_output=True, text=True)
                                self.assertEqual(stamped.returncode == 0, valid, stamped.stderr)
                                if valid:
                                    self.assertIn(f"cargo:rustc-env=DIAL_BUILD_EPOCH={epoch}\n", stamped.stdout)
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

class AurUpdateTests(unittest.TestCase):
    AUR = Path(__file__).resolve().parent.parent / "packaging" / "aur" / "dial-bin"

    def setUp(self):
        self.updater = FormulaUpdateTests.load_updater(self)
        self.pkgbuild = (self.AUR / "PKGBUILD").read_text(encoding="utf-8")
        self.srcinfo = (self.AUR / ".SRCINFO").read_text(encoding="utf-8")

    def committed_release(self):
        """The version and checksums the committed PKGBUILD names. Every release rewrites them
        (release-publish.yml), so a pinned copy here would break on the next release."""
        version = re.search(r"^pkgver=(\d+\.\d+\.\d+)$", self.pkgbuild, re.M).group(1)
        sums = {}
        for arch in ("x86_64", "aarch64"):
            digest = re.search(rf"^sha256sums_{arch}=\('([0-9a-f]{{64}})'\)$", self.pkgbuild, re.M)
            self.assertIsNotNone(digest, f"committed PKGBUILD has no {arch} checksum")
            sums[f"dial-v{version}-{arch}-unknown-linux-gnu.tar.gz"] = digest.group(1)
        return version, sums

    def test_same_release_reproduces_the_committed_files(self):
        # .SRCINFO is regenerated from the PKGBUILD's release, so the two files must agree.
        version, sums = self.committed_release()
        self.assertEqual(self.updater.update_pkgbuild(self.pkgbuild, version, sums), self.pkgbuild)
        self.assertEqual(self.updater.update_srcinfo(self.srcinfo, version, sums), self.srcinfo)

    def test_new_release_updates_version_release_and_every_checksum(self):
        sums = {"dial-v0.4.0-x86_64-unknown-linux-gnu.tar.gz": "1" * 64,
                "dial-v0.4.0-aarch64-unknown-linux-gnu.tar.gz": "2" * 64}
        pkgbuild = self.updater.update_pkgbuild(self.pkgbuild.replace("pkgrel=1", "pkgrel=3"), "0.4.0", sums)
        self.assertIn("pkgver=0.4.0\npkgrel=1\n", pkgbuild)
        self.assertIn(f"sha256sums_x86_64=('{'1' * 64}')", pkgbuild)
        self.assertIn(f"sha256sums_aarch64=('{'2' * 64}')", pkgbuild)
        srcinfo = self.updater.update_srcinfo(self.srcinfo, "0.4.0", sums)
        self.assertNotIn(self.committed_release()[0], srcinfo)
        self.assertIn("\tpkgver = 0.4.0\n\tpkgrel = 1\n", srcinfo)
        self.assertIn("/v0.4.0/dial-v0.4.0-aarch64-unknown-linux-gnu.tar.gz\n\tsha256sums_aarch64 = " + "2" * 64,
                      srcinfo)

    def test_missing_aur_checksum_aborts(self):
        with self.assertRaises(SystemExit):
            self.updater.update_pkgbuild(self.pkgbuild, "0.4.0",
                                         {"dial-v0.4.0-x86_64-unknown-linux-gnu.tar.gz": "1" * 64})



class SourceIgnoreTests(unittest.TestCase):
    def test_source_inputs_remain_visible_but_local_outputs_stay_ignored(self):
        root = Path(__file__).resolve().parent.parent
        sources = [
            "benchmarks/README.md",
            "examples/benchmarks/README.md",
            "examples/benchmarks/new-suite.toml",
            "examples/benchmarks/test_new_suite.py",
            "examples/benchmarks/new-fixture/input.json",
        ]
        outputs = [
            "benchmarks/private-trial/journal.jsonl",
            "target/debug/dial",
            "scratch/report.json",
            "examples/benchmarks/__pycache__/grader.pyc",
            "examples/benchmarks/new-fixture/__pycache__/grader.pyc",
            "examples/benchmarks/.DS_Store",
        ]
        with tempfile.TemporaryDirectory(prefix="dial-source-ignore-") as directory:
            fixture = Path(directory)
            (fixture / ".gitignore").write_bytes((root / ".gitignore").read_bytes())
            subprocess.run(["git", "init", "--quiet", str(fixture)], check=True,
                           capture_output=True)
            result = subprocess.run(
                ["git", "-c", "core.excludesFile=", "check-ignore", "--no-index", "--stdin"],
                cwd=fixture, input="\n".join(sources + outputs) + "\n",
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(set(result.stdout.splitlines()), set(outputs))


class ReleaseWorkflowRuntimeTests(unittest.TestCase):
    WORKFLOW = Path(__file__).resolve().parent.parent / ".github" / "workflows" / "release-build.yml"

    @classmethod
    def build_steps(cls):
        """Maps each job id to its steps' text blocks, using only the workflow's indentation."""
        jobs, job, step = {}, None, None
        for line in cls.WORKFLOW.read_text(encoding="utf-8").splitlines():
            top = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
            if top:
                job, step = top.group(1), None
                jobs[job] = []
                continue
            if job is None:
                continue
            if re.match(r"^      - ", line):
                step = [line]
                jobs[job].append(step)
            elif step is not None and (line.startswith("      ") or not line.strip()):
                step.append(line)
            elif line.strip() and not line.startswith(" "):
                job, step = None, None
        return {name: ["\n".join(block) for block in blocks] for name, blocks in jobs.items()}

    def build_step(self, job):
        matches = [block for block in self.build_steps().get(job, []) if "name: Build release binary" in block]
        self.assertEqual(len(matches), 1, f"{job} must have exactly one build step")
        return matches[0]

    def test_windows_release_binary_links_the_static_c_runtime(self):
        step = self.build_step("windows-build")
        self.assertIn("RUSTFLAGS: '-C target-feature=+crt-static'", step)
        self.assertIn("cargo build --release --locked -p dial", step)

    def test_static_c_runtime_flag_is_scoped_to_the_windows_build(self):
        for job in self.build_steps():
            if job == "windows-build":
                continue
            with self.subTest(job=job):
                self.assertNotIn("crt-static", "\n".join(self.build_steps()[job]))

    def package_step(self, job):
        matches = [block for block in self.build_steps().get(job, []) if "Package and smoke-test" in block]
        self.assertEqual(len(matches), 1, f"{job} must have exactly one package step")
        return matches[0]

    def test_both_build_jobs_prepare_the_pinned_node_payload_before_cargo(self):
        for job in ("build", "windows-build"):
            with self.subTest(job=job):
                blocks = self.build_steps()[job]
                setup = [i for i, block in enumerate(blocks) if "actions/setup-python@" in block]
                prepare = [i for i, block in enumerate(blocks) if "scripts/prepare-node.py --target" in block]
                build = [i for i, block in enumerate(blocks) if "name: Build release binary" in block]
                self.assertEqual((len(setup), len(prepare), len(build)), (1, 1, 1), job)
                self.assertLess(setup[0], prepare[0])
                self.assertLess(prepare[0], build[0])
                self.assertIn("DIAL_BUNDLED_NODE_DIR: ${{ github.workspace }}/target/node-payload/${{ matrix.target }}",
                              self.build_step(job))

    def test_packaged_archives_ship_the_node_license(self):
        for job in ("build", "windows-build"):
            with self.subTest(job=job):
                self.assertIn("LICENSE.node", self.package_step(job))

    def test_installed_probe_is_required_except_on_musl_hosts_that_cannot_run_it(self):
        self.assertIn("--require-bundled-node", self.package_step("windows-build"))
        bash = self.package_step("build")
        self.assertIn('if [[ "$RELEASE_TARGET" == *-musl ]]; then', bash)
        self.assertEqual(bash.count("--require-bundled-node"), 1)
        self.assertIn('--target "$RELEASE_TARGET" --require-bundled-node', bash)

    def test_musl_bundled_runtime_is_verified_on_native_alpine_with_disclosed_libraries(self):
        steps = [block for block in self.build_steps()["build"] if "alpine:" in block]
        self.assertEqual(len(steps), 1)
        for fragment in ("runner.os == 'Linux'", "contains(matrix.target, '-musl')",
                         "apk add --no-cache python3 libstdc++ libgcc", "/checks/scripts:ro",
                         "/checks/runtimes:ro", "/artifact:ro",
                         "python3 /checks/scripts/check-artifact.py --dial /artifact/dial", "--require-bundled-node"):
            with self.subTest(fragment=fragment):
                self.assertIn(fragment, steps[0])
        self.assertNotIn("alpine:", "\n".join(self.build_steps()["windows-build"]))


class InstalledSmokeTests(unittest.TestCase):
    ROOT = Path(__file__).resolve().parent.parent
    PINNED = json.loads((ROOT / "runtimes" / "node" / "manifest.json").read_text(encoding="utf-8"))["version"]
    EXPECTED = f"v{PINNED}"
    REPORT = {"schema_version": 1, "checks": ["bundled-node", "node-version", "computer-worker"],
              "node_version": EXPECTED, "bundled": True}
    # Stands in for dial: asserts the probe's environment contract, then prints the scripted report.
    FAKE = """#!PYTHON
import json, os, sys
from pathlib import Path
behavior = json.loads(Path(__file__).with_name("behavior.json").read_text(encoding="utf-8"))
if os.environ.get("PATH") != "" or os.environ.get("NODE_OPTIONS") != behavior["node_options"] \\
        or os.environ.get("NODE_PATH") != behavior["node_path"]:
    sys.exit(3)
if sys.argv[1:] != ["__installed-smoke", "--require-bundled-node"]:
    sys.exit(4)
home = Path(os.environ["DIAL_HOME"]).resolve()
if home.is_relative_to(Path.cwd().resolve()):
    sys.exit(6)
runs = home.parent / "runs"
count = int(runs.read_text()) + 1 if runs.exists() else 1
runs.write_text(str(count))
cache = home / "payload-cache"
if (count == 1) == cache.exists():
    sys.exit(5)
cache.mkdir(parents=True, exist_ok=True)
print(behavior["stdout"])
sys.exit(behavior["exit"])
"""

    def test_report_must_prove_the_pinned_bundled_payload_and_computer_worker(self):
        cases = {
            "not bundled": ({"bundled": False}, "not bundled"),
            "system node": ({"node_version": "v22.0.0"}, "manifest pins"),
            "no computer worker": ({"checks": ["bundled-node", "node-version"]}, "computer-worker"),
            "checks not a list": ({"checks": "bundled-node computer-worker node-version"}, "did not prove"),
            "unknown schema": ({"schema_version": 2}, "schema"),
        }
        for label, (change, pattern) in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    artifact.check_installed_smoke_report(json.dumps({**self.REPORT, **change}), self.EXPECTED)
        with self.assertRaisesRegex(RuntimeError, "no JSON"):
            artifact.check_installed_smoke_report("probe crashed", self.EXPECTED)
        self.assertEqual(artifact.check_installed_smoke_report(json.dumps(self.REPORT), self.EXPECTED), self.REPORT)
        self.assertEqual(artifact.expected_node_version(), self.EXPECTED)

    @unittest.skipIf(os.name == "nt", "the fake probe is a POSIX shebang script")
    def test_probe_runs_twice_with_empty_path_poison_and_one_home_and_fails_closed(self):
        cases = {
            "fresh run then cached run pass": (json.dumps(self.REPORT), 0, None),
            "missing payload exits nonzero": ("", 1, "failed"),
            "payload not bundled": (json.dumps({**self.REPORT, "bundled": False}), 0, "not bundled"),
        }
        for label, (stdout, exit_code, pattern) in cases.items():
            with self.subTest(case=label), tempfile.TemporaryDirectory(prefix="installed-smoke-test-") as directory:
                root = Path(directory)
                executable = root / "dial"
                executable.write_text(self.FAKE.replace("#!PYTHON", "#!" + sys.executable), encoding="utf-8")
                executable.chmod(0o755)
                (root / "behavior.json").write_text(json.dumps({
                    "node_options": artifact.POISON_NODE_OPTIONS, "node_path": artifact.POISON_NODE_PATH,
                    "stdout": stdout, "exit": exit_code}), encoding="utf-8")
                if pattern is None:
                    self.assertEqual(artifact.installed_smoke(executable, root, self.EXPECTED), self.REPORT)
                else:
                    with self.assertRaisesRegex(RuntimeError, pattern):
                        artifact.installed_smoke(executable, root, self.EXPECTED)


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



class WindowsPortableRuntimeTests(unittest.TestCase):
    """Synthetic PE32+ fixtures: the release .exe must not import the VC++ runtime (normal or delay)."""
    X64 = "x86_64-pc-windows-msvc"
    ARM64 = "aarch64-pc-windows-msvc"
    RUNTIME_DLLS = ("VCRUNTIME140.dll", "vcruntime140_1.dll", "MSVCP140.dll", "concrt140.dll",
                    "ucrtbase.dll", "api-ms-win-crt-runtime-l1-1-0.dll")
    OS_REGULAR = ("KERNEL32.dll", "api-ms-win-core-synch-l1-2-0.dll", "bcrypt.dll", "ntdll.dll")
    OS_DELAYED = ("ws2_32.dll", "api-ms-win-core-path-l1-1-0.dll")
    # Layout: PE header at 0x80, one section ".idata" mapped at RVA 0x1000 from file offset 0x400.
    PE_OFFSET = 0x80
    OPTIONAL = PE_OFFSET + 24
    SECTION_TABLE = OPTIONAL + 240
    BLOB = 0x400
    REGULAR_TABLE = BLOB                # RVA 0x1000
    DELAY_TABLE = BLOB + 0x200          # RVA 0x1200
    NAMES = BLOB + 0x400                # RVA 0x1400

    @classmethod
    def pe(cls, machine=0x8664, magic=0x20B, regular=(), delayed=(), delay_attributes=1):
        data = bytearray(cls.BLOB + 0x800)
        data[0:2] = b"MZ"
        data[0x3C:0x40] = cls.PE_OFFSET.to_bytes(4, "little")
        data[cls.PE_OFFSET:cls.PE_OFFSET + 4] = b"PE\0\0"
        coff = cls.PE_OFFSET + 4
        data[coff:coff + 2] = machine.to_bytes(2, "little")
        data[coff + 2:coff + 4] = (1).to_bytes(2, "little")         # NumberOfSections
        data[coff + 16:coff + 18] = (240).to_bytes(2, "little")     # SizeOfOptionalHeader (PE32+)
        data[cls.OPTIONAL:cls.OPTIONAL + 2] = magic.to_bytes(2, "little")
        data[cls.OPTIONAL + 108:cls.OPTIONAL + 112] = (16).to_bytes(4, "little")
        section = cls.SECTION_TABLE
        data[section:section + 8] = b".idata\0\0"
        data[section + 8:section + 12] = (0x800).to_bytes(4, "little")    # VirtualSize
        data[section + 12:section + 16] = (0x1000).to_bytes(4, "little")  # VirtualAddress
        data[section + 16:section + 20] = (0x800).to_bytes(4, "little")   # SizeOfRawData
        data[section + 20:section + 24] = (cls.BLOB).to_bytes(4, "little")  # PointerToRawData
        names = cls.NAMES

        def name(text):
            nonlocal names
            encoded = text.encode("ascii") + b"\0"
            data[names:names + len(encoded)] = encoded
            rva = 0x1000 + (names - cls.BLOB)
            names += len(encoded)
            return rva.to_bytes(4, "little")

        for index, dll in enumerate(regular):
            entry = cls.REGULAR_TABLE + 20 * index
            data[entry + 12:entry + 16] = name(dll)
        if regular:
            directory = cls.OPTIONAL + 112 + 8 * 1
            data[directory:directory + 4] = (0x1000).to_bytes(4, "little")
            data[directory + 4:directory + 8] = (20 * (len(regular) + 1)).to_bytes(4, "little")
        for index, dll in enumerate(delayed):
            entry = cls.DELAY_TABLE + 32 * index
            data[entry:entry + 4] = delay_attributes.to_bytes(4, "little")
            data[entry + 4:entry + 8] = name(dll)
        if delayed:
            directory = cls.OPTIONAL + 112 + 8 * 13
            data[directory:directory + 4] = (0x1200).to_bytes(4, "little")
            data[directory + 4:directory + 8] = (32 * (len(delayed) + 1)).to_bytes(4, "little")
        return bytes(data)

    def check_pe(self, data, target):
        ArtifactFormatTests.check(self, data, target)

    def test_os_api_set_imports_are_accepted_for_both_windows_architectures(self):
        for machine, target in ((0x8664, self.X64), (0xAA64, self.ARM64)):
            with self.subTest(target=target):
                self.check_pe(self.pe(machine, regular=self.OS_REGULAR, delayed=self.OS_DELAYED), target)
        self.check_pe(self.pe(), self.X64)

    def test_vc_runtime_imports_are_rejected_in_normal_and_delay_tables(self):
        for dll in self.RUNTIME_DLLS:
            for kind in ("regular", "delayed"):
                for machine, target in ((0x8664, self.X64), (0xAA64, self.ARM64)):
                    with self.subTest(dll=dll, kind=kind, target=target):
                        imports = {kind: (dll,)}
                        with self.assertRaisesRegex(RuntimeError, "Visual C\\+\\+ runtime"):
                            self.check_pe(self.pe(machine, **imports), target)

    def test_architecture_and_pe_flavour_are_retained(self):
        with self.assertRaisesRegex(RuntimeError, "is not a PE32\\+ aarch64 binary"):
            self.check_pe(self.pe(0x8664), self.ARM64)
        with self.assertRaisesRegex(RuntimeError, "is not a PE32\\+ x86_64 binary"):
            self.check_pe(self.pe(0xAA64), self.X64)
        with self.assertRaisesRegex(RuntimeError, "is not a PE32\\+ x86_64 binary"):
            self.check_pe(self.pe(magic=0x10B), self.X64)

    def test_malformed_pe_import_tables_fail_closed(self):
        def patched(data, offset, value):
            mutated = bytearray(data)
            mutated[offset:offset + 4] = value.to_bytes(4, "little")
            return bytes(mutated)

        base = self.pe(regular=("KERNEL32.dll",), delayed=("ws2_32.dll",))
        cases = {
            "absolute delay-load descriptor": (self.pe(delayed=("ws2_32.dll",), delay_attributes=0),
                                               "delay-load"),
            "normal import table outside sections": (patched(base, self.OPTIONAL + 112 + 8, 0x9000),
                                                     "outside"),
            "dll name outside sections": (patched(base, self.REGULAR_TABLE + 12, 0x9000 + 0), "outside"),
            "section table missing": (base[:self.SECTION_TABLE], "outside"),
            "truncated file": (base[:0x100], "outside"),
            "zero sections": (base[:self.PE_OFFSET + 6] + (0).to_bytes(2, "little") + base[self.PE_OFFSET + 8:],
                              "outside"),
            "unterminated dll name": (base[:self.NAMES] + b"A" * (len(base) - self.NAMES), "terminated|unterminated"),
        }
        for label, (data, pattern) in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    self.check_pe(data, self.X64)

    def test_unterminated_import_descriptor_table_is_bounded(self):
        from unittest import mock
        data = self.pe(regular=("KERNEL32.dll", "bcrypt.dll", "ntdll.dll"))
        with mock.patch.object(artifact, "MAX_PE_IMPORT_TABLE_ENTRIES", 2):
            with self.assertRaisesRegex(RuntimeError, "terminated|bounded|entries"):
                self.check_pe(data, self.X64)


    @staticmethod
    def patch(data, offset, value, size=4):
        mutated = bytearray(data)
        mutated[offset:offset + size] = value.to_bytes(size, "little")
        return bytes(mutated)

    def test_directory_rva_and_size_must_agree(self):
        base = self.pe(regular=("KERNEL32.dll",))
        directory = self.OPTIONAL + 112 + 8 * 1
        cases = {
            "rva without size": self.patch(base, directory + 4, 0),
            "size without rva": self.patch(base, directory, 0),
        }
        for label, data in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, "inconsistent"):
                    self.check_pe(data, self.X64)

    def test_optional_header_must_hold_the_declared_data_directories(self):
        base = self.pe(regular=("KERNEL32.dll",))
        cases = {
            "optional header smaller than its directories": (self.patch(base, self.PE_OFFSET + 20, 112, 2),
                                                             "optional header"),
            "more directories than the PE format allows": (self.patch(base, self.OPTIONAL + 108, 17),
                                                           "data directories"),
        }
        for label, (data, pattern) in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    self.check_pe(data, self.X64)

    def test_import_terminator_must_fit_inside_the_declared_directory_size(self):
        regular = self.pe(regular=("KERNEL32.dll", "bcrypt.dll"))
        delayed = self.pe(delayed=("ws2_32.dll",))
        cases = {
            "normal table without room for its terminator": self.patch(regular, self.OPTIONAL + 112 + 8 + 4, 40),
            "delay table without room for its terminator": self.patch(delayed, self.OPTIONAL + 112 + 8 * 13 + 4, 32),
        }
        for label, data in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, "declared size"):
                    self.check_pe(data, self.X64)

    def test_descriptors_and_names_stay_inside_their_raw_section(self):
        cases = {
            "descriptor straddles the raw section end": (self.patch(
                self.pe(regular=("KERNEL32.dll",)), self.SECTION_TABLE + 16, 0x10), "crosses"),
            "dll name lies beyond the raw section": (self.patch(
                self.pe(regular=("KERNEL32.dll",)), self.SECTION_TABLE + 16, 0x210), "raw data"),
        }
        for label, (data, pattern) in cases.items():
            with self.subTest(case=label):
                with self.assertRaisesRegex(RuntimeError, pattern):
                    self.check_pe(data, self.X64)


if __name__ == "__main__":
    unittest.main()
