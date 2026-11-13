#!/usr/bin/env python3
"""Prepare the pinned, bundled Node.js payload for one Rust target at build time.

Reads runtimes/node/manifest.json (or --manifest), downloads the official archive for
--target (or uses a local --archive), verifies its SHA-256 before any extraction, extracts
only the exact regular executable and license members, and writes into --output:

  node.gz        deterministic gzip (mtime=0) of the executable
  metadata.txt   exactly four plain lines: target, version, SHA256 of the executable, size in bytes
  LICENSE.node   the Node.js license text

The output directory must not exist; it is assembled in a temporary sibling and renamed into
place, so a failed run never leaves a partial payload behind.
"""
import argparse
import gzip
import hashlib
import hmac
import json
import os
import re
import shutil
import stat
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "runtimes" / "node" / "manifest.json"

DOWNLOAD_LIMIT = 150 * 1024 * 1024
OUTPUT_LIMIT = 256 * 1024 * 1024
DOWNLOAD_TIMEOUT_SECONDS = 120
CHUNK = 1024 * 1024

HEX64 = re.compile(r"^[0-9a-f]{64}$")
# Magic prefixes that the executable must start with for its target's platform.
MAGIC_BY_PLATFORM = {
    "linux": (b"\x7fELF",),
    "windows": (b"MZ",),
    # Mach-O 64-bit little-endian, Mach-O 32-bit, and fat/universal binaries.
    "darwin": (b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe"),
}


class PrepareError(Exception):
    """A refusal with a message fit for the build log; never a partial output."""


def platform_for(triple):
    if "linux" in triple:
        return "linux"
    if "windows" in triple:
        return "windows"
    if "apple-darwin" in triple:
        return "darwin"
    raise PrepareError(f"unsupported target triple for the Node payload: {triple}")


def check_member_name(name):
    if not name or name.startswith("/") or "\\" in name or ":" in name.split("/", 1)[0]:
        raise PrepareError(f"manifest member name is not a relative archive path: {name!r}")
    if any(part in ("", ".", "..") for part in name.split("/")):
        raise PrepareError(f"manifest member name has an empty or traversal component: {name!r}")
    return name


def load_manifest(path):
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise PrepareError(f"cannot read manifest {path}: {error}") from error
    if not isinstance(data, dict) or not isinstance(data.get("version"), str) or not data["version"]:
        raise PrepareError("manifest needs a non-empty string 'version'")
    targets = data.get("targets")
    if not isinstance(targets, dict) or not targets:
        raise PrepareError("manifest needs a non-empty 'targets' object")
    for triple, entry in targets.items():
        if not isinstance(entry, dict):
            raise PrepareError(f"manifest target {triple} must be an object")
        url = entry.get("url")
        if not isinstance(url, str) or not url.startswith("https://"):
            raise PrepareError(f"manifest target {triple} needs an https 'url'")
        digest = entry.get("sha256")
        if not isinstance(digest, str) or not HEX64.match(digest):
            raise PrepareError(f"manifest target {triple} needs a lowercase hex 'sha256'")
        check_member_name(entry.get("binary", ""))
        check_member_name(entry.get("license", ""))
    return data


def archive_kind(name):
    lowered = name.lower()
    if lowered.endswith((".tar.xz", ".tar.gz", ".tgz")):
        return "tar"
    if lowered.endswith(".zip"):
        return "zip"
    raise PrepareError(f"unsupported archive type (expected .tar.xz, .tar.gz, .tgz or .zip): {name}")


def download(url, destination):
    """Stream the archive to destination, refusing anything over DOWNLOAD_LIMIT."""
    digest = hashlib.sha256()
    total = 0
    try:
        with urllib.request.urlopen(url, timeout=DOWNLOAD_TIMEOUT_SECONDS) as response, \
                open(destination, "wb") as out:
            declared = response.headers.get("Content-Length")
            if declared and declared.isdigit() and int(declared) > DOWNLOAD_LIMIT:
                raise PrepareError(f"archive declares {declared} bytes, over the {DOWNLOAD_LIMIT} limit")
            while True:
                chunk = response.read(CHUNK)
                if not chunk:
                    break
                total += len(chunk)
                if total > DOWNLOAD_LIMIT:
                    raise PrepareError(f"archive exceeds the {DOWNLOAD_LIMIT} byte download limit")
                out.write(chunk)
    except OSError as error:
        raise PrepareError(f"download failed for {url}: {error}") from error
    return total


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(CHUNK), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_bounded(stream, name):
    data = stream.read(OUTPUT_LIMIT + 1)
    if len(data) > OUTPUT_LIMIT:
        raise PrepareError(f"{name} inflates past the {OUTPUT_LIMIT} byte output limit")
    return data


def extract_tar(archive, wanted):
    found = {}
    try:
        with tarfile.open(archive, "r:*") as tar:
            for member in tar:
                name = member.name
                if name.startswith("/") or ".." in name.split("/"):
                    raise PrepareError(f"archive contains an unsafe member path: {name!r}")
                if name not in wanted:
                    continue
                if name in found:
                    raise PrepareError(f"archive contains duplicate member: {name}")
                if member.issym() or member.islnk():
                    raise PrepareError(f"archive member is a link, not a regular file: {name}")
                if not member.isreg():
                    raise PrepareError(f"archive member is not a regular file: {name}")
                if member.size > OUTPUT_LIMIT:
                    raise PrepareError(f"archive member {name} is {member.size} bytes, over the limit")
                handle = tar.extractfile(member)
                if handle is None:
                    raise PrepareError(f"archive member cannot be read: {name}")
                found[name] = read_bounded(handle, name)
    except tarfile.TarError as error:
        raise PrepareError(f"cannot read tar archive: {error}") from error
    return found


def extract_zip(archive, wanted):
    found = {}
    try:
        with zipfile.ZipFile(archive) as zf:
            seen = {}
            for info in zf.infolist():
                name = info.filename
                if name.startswith("/") or "\\" in name or ".." in name.split("/"):
                    raise PrepareError(f"archive contains an unsafe member path: {name!r}")
                seen[name] = seen.get(name, 0) + 1
            for name in wanted:
                if seen.get(name, 0) > 1:
                    raise PrepareError(f"archive contains duplicate member: {name}")
            for info in zf.infolist():
                name = info.filename
                if name not in wanted:
                    continue
                # Unix mode bits; zero or permission-only bits mean "no type recorded" (regular).
                mode = info.external_attr >> 16
                kind_bits = stat.S_IFMT(mode)
                if stat.S_ISLNK(mode):
                    raise PrepareError(f"archive member is a symlink, not a regular file: {name}")
                if kind_bits and not stat.S_ISREG(mode):
                    raise PrepareError(f"archive member is not a regular file: {name}")
                if info.is_dir():
                    raise PrepareError(f"archive member is not a regular file: {name}")
                if info.file_size > OUTPUT_LIMIT:
                    raise PrepareError(f"archive member {name} is {info.file_size} bytes, over the limit")
                with zf.open(info) as handle:
                    found[name] = read_bounded(handle, name)
    except zipfile.BadZipFile as error:
        raise PrepareError(f"cannot read zip archive: {error}") from error
    return found


def extract_members(archive, kind, wanted):
    """Return {member name: bytes} for exactly the wanted members; refuse anything else odd."""
    found = extract_tar(archive, wanted) if kind == "tar" else extract_zip(archive, wanted)
    missing = [name for name in wanted if name not in found]
    if missing:
        raise PrepareError(f"archive is missing member(s): {', '.join(missing)}")
    return found


def check_license(data, name):
    if not data.strip():
        raise PrepareError(f"license member {name} is empty")


def check_executable(data, triple, name):
    if not data:
        raise PrepareError(f"executable member {name} is empty")
    magics = MAGIC_BY_PLATFORM[platform_for(triple)]
    if not data.startswith(magics):
        raise PrepareError(f"executable member {name} is not a {platform_for(triple)} binary")


def metadata_text(triple, version, executable):
    """The four plain lines the build consumes: target, version, SHA256, size."""
    return (
        f"{triple}\n"
        f"{version}\n"
        f"{hashlib.sha256(executable).hexdigest()}\n"
        f"{len(executable)}\n"
    )


def build_payload(archive_path, kind, entry, triple, version):
    binary, license_name = entry["binary"], entry["license"]
    if binary == license_name:
        raise PrepareError("manifest binary and license must be different members")
    members = extract_members(archive_path, kind, [binary, license_name])
    executable, license_text = members[binary], members[license_name]
    check_executable(executable, triple, binary)
    check_license(license_text, license_name)
    gz = gzip.compress(executable, compresslevel=9, mtime=0)
    # Roundtrip before anything is written: the compressed bytes must restore exactly.
    if gzip.decompress(gz) != executable:
        raise PrepareError("gzip roundtrip of the executable did not reproduce its bytes")
    metadata = metadata_text(triple, version, executable)
    return gz, metadata, license_text


def write_output(output, gz, metadata, license_text, triple, version):
    output = Path(output)
    if output.exists() or output.is_symlink():
        raise PrepareError(f"output already exists, refusing to overwrite: {output}")
    parent = output.parent
    parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".{output.name}.", suffix=".partial", dir=parent))
    try:
        (staging / "node.gz").write_bytes(gz)
        (staging / "metadata.txt").write_text(metadata, encoding="utf-8", newline="\n")
        (staging / "LICENSE.node").write_bytes(license_text)
        verify_output(staging, triple, version)
        os.rename(staging, output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise


def verify_output(directory, triple, version):
    """Read the written payload back and check it against its own metadata."""
    gz = (directory / "node.gz").read_bytes()
    executable = gzip.decompress(gz)
    metadata = (directory / "metadata.txt").read_text(encoding="utf-8")
    if metadata != metadata_text(triple, version, executable):
        raise PrepareError("metadata.txt does not match the payload it describes")
    if not (directory / "LICENSE.node").stat().st_size:
        raise PrepareError("LICENSE.node is empty")


def prepare(target, output, manifest_path=DEFAULT_MANIFEST, archive=None):
    manifest = load_manifest(manifest_path)
    targets = manifest["targets"]
    if target not in targets:
        raise PrepareError(f"target {target} is not in the manifest (have: {', '.join(sorted(targets))})")
    entry = targets[target]
    version = manifest["version"]
    name_source = archive if archive is not None else entry["url"].rsplit("/", 1)[-1]
    kind = archive_kind(str(name_source))
    if archive is not None:
        archive_path = Path(archive)
        if not archive_path.is_file():
            raise PrepareError(f"archive not found: {archive}")
        actual = sha256_file(archive_path)
        if not hmac.compare_digest(actual, entry["sha256"]):
            raise PrepareError(f"archive SHA-256 mismatch for {archive}: expected {entry['sha256']}, got {actual}")
        gz, metadata, license_text = build_payload(archive_path, kind, entry, target, version)
    else:
        with tempfile.TemporaryDirectory(prefix="prepare-node-") as scratch:
            archive_path = Path(scratch) / "archive"
            download(entry["url"], archive_path)
            actual = sha256_file(archive_path)
            if not hmac.compare_digest(actual, entry["sha256"]):
                raise PrepareError(f"download SHA-256 mismatch for {entry['url']}: expected {entry['sha256']}, got {actual}")
            gz, metadata, license_text = build_payload(archive_path, kind, entry, target, version)
    write_output(output, gz, metadata, license_text, target, version)
    return Path(output)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", required=True, help="Rust target triple listed in the manifest")
    parser.add_argument("--output", required=True, help="output directory; must not already exist")
    parser.add_argument("--archive", help="local archive to use instead of downloading (offline)")
    parser.add_argument("--manifest", default=str(DEFAULT_MANIFEST), help="manifest path")
    args = parser.parse_args(argv)
    try:
        out = prepare(args.target, args.output, Path(args.manifest), args.archive)
    except PrepareError as error:
        print(f"prepare-node: error: {error}", file=sys.stderr)
        return 1
    print(f"prepare-node: wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
