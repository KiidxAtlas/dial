#!/usr/bin/env python3
"""Offline regression tests for scripts/prepare-node.py (no network; synthetic archives only)."""
import gzip
import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
import unittest.mock
import warnings
import zipfile
from pathlib import Path


def load_preparer():
    # Hyphenated script (like scripts/test-packaging.py), so load it by path.
    path = Path(__file__).resolve().with_name("prepare-node.py")
    spec = importlib.util.spec_from_file_location("prepare_node", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare_node = load_preparer()

TRIPLE = "x86_64-unknown-linux-gnu"
VERSION = "24.22.3"
PREFIX = f"node-v{VERSION}-linux-x64"
BINARY = f"{PREFIX}/bin/node"
LICENSE = f"{PREFIX}/LICENSE"
ELF_BODY = b"\x7fELF" + b"node-binary-fixture" * 64
LICENSE_BODY = b"Node.js license fixture\n"


def archive_entries(binary=ELF_BODY, license_body=LICENSE_BODY):
    return [(BINARY, binary), (LICENSE, license_body)]


def write_tar(path, entries, mode="w:gz"):
    """entries: (name, bytes) regular files, or (name, tarfile type constant, linkname)."""
    with tarfile.open(path, mode) as tar:
        for entry in entries:
            name = entry[0]
            info = tarfile.TarInfo(name)
            if len(entry) == 3:
                info.type, info.linkname = entry[1], entry[2]
                info.size = 0
                tar.addfile(info)
                continue
            data = entry[1]
            if data is None:  # directory entry
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
                continue
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return path


def write_zip(path, entries):
    """entries: (name, bytes) or (name, bytes, unix_mode) to set external_attr."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", UserWarning)  # duplicate-name fixture is intentional
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
            for entry in entries:
                info = zipfile.ZipInfo(entry[0], date_time=(1980, 1, 1, 0, 0, 0))
                if len(entry) == 3:
                    info.external_attr = entry[2] << 16
                    info.create_system = 3
                zf.writestr(info, entry[1])
    return path


def sha256_of(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class PrepareNodeTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.tar = write_tar(self.root / "node.tar.gz", archive_entries())
        self.zip = write_zip(self.root / "node.zip", archive_entries())
        self.write_manifest(url_name="node.tar.gz", archive=self.tar)

    def write_manifest(self, url_name="node.tar.gz", archive=None, sha=None, binary=BINARY,
                       license_name=LICENSE, url=None, version=VERSION):
        digest = sha if sha is not None else sha256_of(archive or self.tar)
        manifest = {
            "version": version,
            "targets": {
                TRIPLE: {
                    "url": url or f"https://nodejs.org/dist/v{VERSION}/{url_name}",
                    "sha256": digest,
                    "binary": binary,
                    "license": license_name,
                }
            },
        }
        self.manifest = self.root / "manifest.json"
        self.manifest.write_text(json.dumps(manifest), encoding="utf-8")
        return self.manifest

    def output(self, name="payload"):
        return self.root / "out" / name

    def run_prepare(self, archive=None, output=None, target=TRIPLE):
        return prepare_node.prepare(target, output or self.output(), self.manifest,
                                    archive=archive)

    def assert_refused(self, message, archive=None, output=None, target=TRIPLE):
        with self.assertRaisesRegex(prepare_node.PrepareError, message):
            self.run_prepare(archive=archive, output=output, target=target)
        self.assertFalse((output or self.output()).exists(), "refusal must not create output")

    # Known answers ---------------------------------------------------------

    def test_tar_and_zip_produce_identical_known_answer_payload(self):
        expected_meta = (
            f"{TRIPLE}\n{VERSION}\n"
            f"{hashlib.sha256(ELF_BODY).hexdigest()}\n"
            f"{len(ELF_BODY)}\n"
        )
        for kind, archive in (("tar", self.tar), ("zip", self.zip)):
            with self.subTest(kind=kind):
                self.write_manifest(url_name="node.tar.gz" if kind == "tar" else "node.zip",
                                    archive=archive)
                out = self.run_prepare(archive=archive, output=self.output(kind))
                self.assertEqual(sorted(p.name for p in out.iterdir()),
                                 ["LICENSE.node", "metadata.txt", "node.gz"])
                self.assertEqual(gzip.decompress((out / "node.gz").read_bytes()), ELF_BODY)
                self.assertEqual((out / "metadata.txt").read_text(encoding="utf-8"), expected_meta)
                self.assertEqual((out / "metadata.txt").read_text().splitlines(),
                                 [TRIPLE, VERSION, hashlib.sha256(ELF_BODY).hexdigest(), str(len(ELF_BODY))])
                self.assertEqual((out / "LICENSE.node").read_bytes(), LICENSE_BODY)

    def test_payload_is_deterministic_across_runs(self):
        first = self.run_prepare(archive=self.tar, output=self.output("a"))
        second = self.run_prepare(archive=self.tar, output=self.output("b"))
        for name in ("node.gz", "metadata.txt", "LICENSE.node"):
            with self.subTest(name=name):
                self.assertEqual((first / name).read_bytes(), (second / name).read_bytes())
        # mtime=0 means the gzip header carries no timestamp.
        self.assertEqual((first / "node.gz").read_bytes()[4:8], b"\x00\x00\x00\x00")

    # Digest acceptance -----------------------------------------------------

    def test_digest_mismatch_with_local_archive_refuses_without_output(self):
        self.write_manifest(archive=self.tar, sha="0" * 64)
        with unittest.mock.patch.object(prepare_node, "extract_members") as extract:
            self.assert_refused("SHA-256 mismatch", archive=self.tar)
        extract.assert_not_called()

    def test_digest_mismatch_on_download_never_extracts(self):
        self.write_manifest(archive=self.tar, sha="f" * 64)

        def fake_download(url, destination):
            Path(destination).write_bytes(self.tar.read_bytes())
            return destination.stat().st_size

        with unittest.mock.patch.object(prepare_node, "download", side_effect=fake_download), \
                unittest.mock.patch.object(prepare_node, "extract_members") as extract:
            self.assert_refused("download SHA-256 mismatch")
        extract.assert_not_called()

    def test_download_path_extracts_only_after_digest_matches(self):
        def fake_download(url, destination):
            Path(destination).write_bytes(self.tar.read_bytes())
            return destination.stat().st_size

        with unittest.mock.patch.object(prepare_node, "download", side_effect=fake_download):
            out = self.run_prepare()
        self.assertEqual(gzip.decompress((out / "node.gz").read_bytes()), ELF_BODY)

    def test_missing_archive_file_refuses(self):
        self.assert_refused("archive not found", archive=self.root / "absent.tar.gz")

    # Member selection and hostile archives --------------------------------

    def test_archive_bad_member_shapes_are_refused(self):
        cases = {
            "missing binary": ("missing member", [(LICENSE, LICENSE_BODY)], "tar"),
            "missing license": ("missing member", [(BINARY, ELF_BODY)], "tar"),
            "duplicate binary": ("duplicate member",
                                 [(BINARY, ELF_BODY), (BINARY, ELF_BODY), (LICENSE, LICENSE_BODY)], "tar"),
            "symlink binary": ("link, not a regular file",
                               [(BINARY, tarfile.SYMTYPE, "/bin/sh"), (LICENSE, LICENSE_BODY)], "tar"),
            "hardlink binary": ("link, not a regular file",
                                [(BINARY, tarfile.LNKTYPE, LICENSE), (LICENSE, LICENSE_BODY)], "tar"),
            "directory binary": ("not a regular file",
                                 [(BINARY, None), (LICENSE, LICENSE_BODY)], "tar"),
            "traversal member": ("unsafe member path",
                                 [(BINARY, ELF_BODY), (LICENSE, LICENSE_BODY), ("../evil", b"x")], "tar"),
            "absolute member": ("unsafe member path",
                                [("/etc/node", b"x"), (BINARY, ELF_BODY), (LICENSE, LICENSE_BODY)], "tar"),
            "zip missing binary": ("missing member", [(LICENSE, LICENSE_BODY)], "zip"),
            "zip duplicate license": ("duplicate member",
                                      [(BINARY, ELF_BODY), (LICENSE, LICENSE_BODY), (LICENSE, b"x")], "zip"),
            "zip symlink binary": ("symlink, not a regular file",
                                   [(BINARY, b"/bin/sh", 0o120777), (LICENSE, LICENSE_BODY)], "zip"),
            "zip directory binary": ("not a regular file",
                                     [(BINARY, ELF_BODY, 0o040755), (LICENSE, LICENSE_BODY)], "zip"),
            "zip traversal member": ("unsafe member path",
                                     [(BINARY, ELF_BODY), (LICENSE, LICENSE_BODY), ("../x", b"x")], "zip"),
        }
        for label, (message, entries, kind) in cases.items():
            with self.subTest(case=label):
                if kind == "tar":
                    archive = write_tar(self.root / f"{label.replace(' ', '_')}.tar.gz", entries)
                    url_name = "node.tar.gz"
                else:
                    archive = write_zip(self.root / f"{label.replace(' ', '_')}.zip", entries)
                    url_name = "node.zip"
                self.write_manifest(url_name=url_name, archive=archive)
                self.assert_refused(message, archive=archive, output=self.output(label.replace(" ", "_")))

    def test_member_bound_refuses_oversize_output(self):
        small_limit = 1024
        big = b"\x7fELF" + b"A" * small_limit
        archive = write_tar(self.root / "big.tar.gz", archive_entries(binary=big))
        self.write_manifest(archive=archive)
        for limit_name in ("OUTPUT_LIMIT",):
            with self.subTest(limit=limit_name), \
                    unittest.mock.patch.object(prepare_node, limit_name, small_limit):
                self.assert_refused("over the", archive=archive)
        zip_archive = write_zip(self.root / "big.zip", archive_entries(binary=big))
        self.write_manifest(url_name="node.zip", archive=zip_archive)
        with unittest.mock.patch.object(prepare_node, "OUTPUT_LIMIT", small_limit):
            self.assert_refused("over the limit", archive=zip_archive)

    def test_download_bound_refuses_stream_over_limit(self):
        class FakeResponse(io.BytesIO):
            headers = {}

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        def fake_urlopen(url, timeout):
            return FakeResponse(b"x" * 4096)

        destination = self.root / "dl.bin"
        with unittest.mock.patch.object(prepare_node, "DOWNLOAD_LIMIT", 1000), \
                unittest.mock.patch.object(prepare_node.urllib.request, "urlopen", side_effect=fake_urlopen):
            with self.assertRaisesRegex(prepare_node.PrepareError, "download limit"):
                prepare_node.download("https://nodejs.org/dist/x.tar.gz", destination)

    def test_download_uses_timeout(self):
        seen = {}

        class Empty(io.BytesIO):
            headers = {}

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        def fake_urlopen(url, timeout):
            seen["timeout"] = timeout
            return Empty(b"")

        with unittest.mock.patch.object(prepare_node.urllib.request, "urlopen", side_effect=fake_urlopen):
            prepare_node.download("https://nodejs.org/dist/x.tar.gz", self.root / "e.bin")
        self.assertGreater(seen["timeout"], 0)

    # Payload validation ----------------------------------------------------

    def test_wrong_platform_magic_is_refused(self):
        archive = write_tar(self.root / "pe.tar.gz", archive_entries(binary=b"MZ" + b"x" * 32))
        self.write_manifest(archive=archive)
        self.assert_refused("not a linux binary", archive=archive)

    def test_empty_executable_and_license_are_refused(self):
        for label, entries, message in (
            ("empty binary", archive_entries(binary=b""), "executable member .* is empty"),
            ("empty license", archive_entries(license_body=b"  \n"), "license member .* is empty"),
        ):
            with self.subTest(case=label):
                archive = write_tar(self.root / f"{label.replace(' ', '_')}.tar.gz", entries)
                self.write_manifest(archive=archive)
                self.assert_refused(message, archive=archive, output=self.output(label.replace(" ", "_")))

    def test_unknown_target_and_platform_are_refused(self):
        self.assert_refused("not in the manifest", target="riscv64gc-unknown-hurd")
        self.write_manifest(archive=self.tar)
        manifest = json.loads(self.manifest.read_text())
        manifest["targets"]["riscv64gc-unknown-hurd"] = dict(manifest["targets"][TRIPLE])
        self.manifest.write_text(json.dumps(manifest))
        self.assert_refused("unsupported target triple", archive=self.tar,
                            target="riscv64gc-unknown-hurd")

    def test_manifest_validation_refuses_bad_entries(self):
        cases = {
            "http url": {"url": "http://nodejs.org/dist/x.tar.gz"},
            "short digest": {"sha256": "abc"},
            "uppercase digest": {"sha256": "A" * 64},
            "absolute binary": {"binary": "/bin/node"},
            "traversal license": {"license": "../LICENSE"},
            "backslash binary": {"binary": "pkg\\bin\\node"},
        }
        base = json.loads(self.manifest.read_text())
        for label, override in cases.items():
            with self.subTest(case=label):
                manifest = json.loads(json.dumps(base))
                manifest["targets"][TRIPLE].update(override)
                self.manifest.write_text(json.dumps(manifest))
                self.assert_refused("manifest|https|hex|relative|traversal|empty",
                                    archive=self.tar)
        self.manifest.write_text("[]")
        self.assert_refused("needs a non-empty string 'version'", archive=self.tar)

    # Output safety ---------------------------------------------------------

    def test_existing_output_is_never_overwritten(self):
        existing = self.output("taken")
        existing.mkdir(parents=True)
        (existing / "keep.txt").write_text("keep")
        with self.assertRaisesRegex(prepare_node.PrepareError, "already exists"):
            self.run_prepare(archive=self.tar, output=existing)
        self.assertEqual((existing / "keep.txt").read_text(), "keep")

    def test_failed_verification_leaves_no_partial_directory(self):
        out = self.output("partial")
        with unittest.mock.patch.object(prepare_node, "verify_output", side_effect=prepare_node.PrepareError("boom")):
            self.assert_refused("boom", archive=self.tar, output=out)
        leftovers = [p.name for p in out.parent.iterdir()] if out.parent.exists() else []
        self.assertEqual(leftovers, [])

    def test_verify_output_detects_tampered_metadata(self):
        out = self.run_prepare(archive=self.tar, output=self.output("tamper"))
        (out / "metadata.txt").write_text("target=x\n")
        with self.assertRaisesRegex(prepare_node.PrepareError, "metadata.txt does not match"):
            prepare_node.verify_output(out, TRIPLE, VERSION)

    def test_cli_reports_errors_with_exit_status(self):
        self.write_manifest(archive=self.tar, sha="0" * 64)
        code = prepare_node.main(["--target", TRIPLE, "--output", str(self.output("cli")),
                                  "--archive", str(self.tar), "--manifest", str(self.manifest)])
        self.assertEqual(code, 1)
        self.assertFalse(self.output("cli").exists())


EXPECTED_TRIPLES = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
}
ALLOWED_HOSTS = {"nodejs.org", "unofficial-builds.nodejs.org"}


class RepositoryManifestTest(unittest.TestCase):
    """Structural contract for the committed runtimes/node/manifest.json.

    Checks shape and internal consistency only: no network, and no pinned digest is
    transcribed into the test, so regenerating the manifest does not edit this file.
    """

    @classmethod
    def setUpClass(cls):
        cls.manifest_path = prepare_node.DEFAULT_MANIFEST
        cls.manifest = prepare_node.load_manifest(cls.manifest_path)

    def test_covers_exactly_the_eight_release_triples(self):
        self.assertEqual(set(self.manifest["targets"]), EXPECTED_TRIPLES)

    def test_version_is_a_plain_semver_string(self):
        self.assertRegex(self.manifest["version"], r"^[0-9]+\.[0-9]+\.[0-9]+$")

    def test_every_entry_is_consistent_and_pinned(self):
        version = self.manifest["version"]
        for triple, entry in sorted(self.manifest["targets"].items()):
            with self.subTest(triple=triple):
                self.assertEqual(set(entry), {"url", "sha256", "binary", "license"})
                url = entry["url"]
                host = url.split("/", 3)[2]
                self.assertIn(host, ALLOWED_HOSTS)
                self.assertIn(f"/v{version}/", url)
                self.assertNotIn("?", url)
                self.assertNotIn("#", url)
                self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")
                filename = url.rsplit("/", 1)[-1]
                self.assertTrue(filename.startswith(f"node-v{version}-"), filename)
                kind = prepare_node.archive_kind(filename)
                platform = prepare_node.platform_for(triple)
                self.assertEqual(kind, "zip" if platform == "windows" else "tar")
                suffix = ".tar.gz" if kind == "tar" else ".zip"
                stem = filename[:-len(suffix)]
                self.assertTrue(stem.startswith(f"node-v{version}-"), stem)
                self.assertTrue(entry["binary"].startswith(f"{stem}/"), entry["binary"])
                self.assertTrue(entry["license"].startswith(f"{stem}/"), entry["license"])
                self.assertEqual(entry["license"].rsplit("/", 1)[-1], "LICENSE")
                binary_name = entry["binary"].rsplit("/", 1)[-1]
                self.assertEqual(binary_name, "node.exe" if platform == "windows" else "node")

    def test_unofficial_host_is_used_only_for_musl_arm64(self):
        unofficial = {t for t, e in self.manifest["targets"].items()
                      if e["url"].startswith("https://unofficial-builds.nodejs.org/")}
        self.assertEqual(unofficial, {"aarch64-unknown-linux-musl"})


if __name__ == "__main__":
    unittest.main()
