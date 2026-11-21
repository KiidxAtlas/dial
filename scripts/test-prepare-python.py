#!/usr/bin/env python3
"""Offline tests at the upstream archive -> embedded CPython payload boundary."""
import gzip
import hashlib
import importlib.util
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import runtime_payload

spec = importlib.util.spec_from_file_location("prepare_python", Path(__file__).with_name("prepare-python.py"))
prep = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prep)

TARGET = "aarch64-unknown-linux-gnu"
VERSION = "3.13.18"
BINARY = "bin/python3.13"
ALIAS = "bin/python3"
LICENSE = "lib/python3.13/LICENSE.txt"


class PythonPayloadTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.archive = self.root / "source.tar.gz"
        self.manifest = self.root / "manifest.json"
        self.out = self.root / "payload"

    def fixture(self, extra=(), bad_digest=False, windows=False):
        entries = [(BINARY, b"python-binary"), (ALIAS, "python3.13"), (LICENSE, b"psf license")]
        if windows:
            entries = [("python.exe", b"python-binary"), ("LICENSE.txt", b"psf license")]
        with tarfile.open(self.archive, "w:gz") as tar:
            for name, value in [*entries, *extra]:
                m = tarfile.TarInfo("python/" + name)
                m.mode = 0o4755
                if isinstance(value, bytes):
                    m.size = len(value)
                    tar.addfile(m, io.BytesIO(value))
                else:
                    m.type = tarfile.SYMTYPE
                    m.linkname = value
                    tar.addfile(m)
        digest = "0" * 64 if bad_digest else hashlib.sha256(self.archive.read_bytes()).hexdigest()
        target = "aarch64-pc-windows-msvc" if windows else TARGET
        self.manifest.write_text(json.dumps({"version": VERSION, "targets": {target: {
            "url": "https://invalid.example/python.tar.gz", "sha256": digest}}}))
        return target

    def test_offline_regular_files_metadata_and_licenses(self):
        for windows in (True, False):
            with self.subTest(windows=windows), \
                    patch.object(runtime_payload, "download", side_effect=AssertionError("network")):
                target = self.fixture(windows=windows)
                output = self.root / target
                prep.prepare(target, output, self.archive, self.manifest)
                raw = gzip.decompress((output / "python.tar.gz").read_bytes())
                self.assertEqual((output / "metadata.txt").read_text().splitlines(),
                                 [target, VERSION, hashlib.sha256(raw).hexdigest(), str(len(raw))])
                with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as tar:
                    binary = "python.exe" if windows else ALIAS
                    self.assertEqual(tar.extractfile(binary).read(), b"python-binary")
                    self.assertEqual(tar.getmember(binary).mode, 0o755)
                    self.assertTrue(all(m.isfile() for m in tar))
                    self.assertIn(tar.extractfile(prep.NOTICES).read(), (output / "LICENSE.python").read_bytes())
                    self.assertIn(b"psf license", (output / "LICENSE.python").read_bytes())
                with self.assertRaisesRegex(ValueError, "already exists"):
                    prep.prepare(target, output, self.archive, self.manifest)

    def test_digest_must_match_before_decompression(self):
        self.fixture(bad_digest=True)
        with patch.object(prep.gzip, "open", side_effect=AssertionError("decompressed unverified input")):
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                prep.prepare(TARGET, self.out, self.archive, self.manifest)
        self.assertFalse(self.out.exists())

    def test_malformed_verified_archives_never_publish_a_partial_payload(self):
        for extra in [[("../escape", b"bad")], [("bin/bad", "../../outside")], [("bin/loop", "loop")],
                      [(BINARY, b"duplicate")], [(ALIAS + "-bad", "missing")]]:
            with self.subTest(extra=extra):
                self.fixture(extra)
                with self.assertRaises(ValueError):
                    prep.prepare(TARGET, self.out, self.archive, self.manifest)
                self.assertFalse(self.out.exists())
                self.assertFalse((self.root / "escape").exists())


if __name__ == "__main__":
    unittest.main()
