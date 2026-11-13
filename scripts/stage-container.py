#!/usr/bin/env python3
"""Stage the signed Linux release archive for a compiler-free container build."""
import base64
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

FILES = {"dial", "LICENSE", "LICENSE.node", "README.md", "THIRD_PARTY_NOTICES", "Silver.LICENSE", "Unscii.LICENSE"}


def signature_verifies(public_key, signature, manifest):
    """Verify an Ed25519 signature, preferring Python cryptography over the openssl CLI.

    Some platforms' `openssl` (LibreSSL, older OpenSSL) cannot verify raw Ed25519 input.
    """
    try:
        from cryptography.exceptions import InvalidSignature
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
    except ImportError:
        pass
    else:
        try:
            key = serialization.load_pem_public_key(public_key.encode("utf-8"))
        except ValueError:
            return False
        if not isinstance(key, Ed25519PublicKey):
            return False
        try:
            key.verify(signature, manifest)
        except InvalidSignature:
            return False
        return True
    with tempfile.TemporaryDirectory(prefix="dial-manifest-") as temporary:
        temporary = Path(temporary)
        (temporary / "public.pem").write_text(public_key, encoding="utf-8")
        (temporary / "signature").write_bytes(signature)
        (temporary / "manifest").write_bytes(manifest)
        result = subprocess.run([
            os.environ.get("DIAL_OPENSSL", "openssl"), "pkeyutl", "-verify", "-pubin",
            "-inkey", str(temporary / "public.pem"), "-rawin",
            "-in", str(temporary / "manifest"), "-sigfile", str(temporary / "signature"),
        ], capture_output=True)
    return result.returncode == 0


def stage(dist, destination, tag, public_key):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("expected a stable release tag")
    if destination.exists():
        raise ValueError("container context must not already exist")
    manifest = (dist / "SHA256SUMS").read_bytes()
    signature = base64.b64decode((dist / "SHA256SUMS.sig").read_bytes(), validate=True)
    if not signature_verifies(public_key, signature, manifest):
        raise ValueError("release manifest signature did not verify")
    package = f"dial-{tag}-x86_64-unknown-linux-gnu"
    archive = f"{package}.tar.gz"
    entries = []
    for line in manifest.decode("utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if match is None:
            raise ValueError("malformed release manifest")
        entries.append(match.groups())
    digests = [digest for digest, name in entries if name == archive]
    if len(digests) != 1:
        raise ValueError("manifest must list the Linux archive exactly once")
    with (dist / archive).open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != digests[0]:
        raise ValueError("Linux archive checksum did not match signed manifest")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="dial-container-", dir=destination.parent) as temporary:
        temporary = Path(temporary)
        seen = set()
        with tarfile.open(dist / archive, "r:gz") as source:
            for member in source:
                if member.isdir() and member.name.rstrip("/") == package:
                    continue
                prefix = f"{package}/"
                name = member.name[len(prefix):] if member.name.startswith(prefix) else ""
                limit = 512 * 1024 * 1024 if name == "dial" else 16 * 1024 * 1024
                if name not in FILES or name in seen or not member.isfile() or member.size > limit:
                    raise ValueError("unexpected, duplicate or unsafe Linux archive member")
                seen.add(name)
                with source.extractfile(member) as contents, (temporary / name).open("wb") as output:
                    shutil.copyfileobj(contents, output)
                (temporary / name).chmod(0o755 if name == "dial" else 0o644)
        if seen != FILES:
            raise ValueError("Linux archive is missing required binary or notices")
        temporary.rename(destination)


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: stage-container.py DIST DESTINATION")
    try:
        stage(Path(sys.argv[1]), Path(sys.argv[2]), os.environ["RELEASE_TAG"],
              os.environ["DIAL_RELEASE_PUBLIC_KEY_PEM"])
    except (OSError, ValueError, tarfile.TarError) as error:
        raise SystemExit(f"container staging: {error}") from error


if __name__ == "__main__":
    main()
