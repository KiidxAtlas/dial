"""Bounded, verified archive acquisition for pinned runtime payloads (build time only).

Every payload archive goes through snapshot_archive: it is copied (from an https download or a
caller-owned local file) into a private path under the caller's scratch directory, bounded by
DOWNLOAD_LIMIT, and its SHA-256 is checked on that copy. Extraction reads only the verified
snapshot, never the original path, so a file that changes after hashing cannot reach the extractor.
"""
import hashlib
import hmac
import urllib.request
from pathlib import Path

CHUNK = 1024 * 1024
DOWNLOAD_LIMIT = 150 * 1024 * 1024


class PrepareError(ValueError):
    """A refusal with a message suitable for the build log."""


def copy_bounded(src, dst, limit, message):
    """Copy src to dst in chunks; refuse with message once more than limit bytes have been read."""
    total = 0
    while chunk := src.read(CHUNK):
        total += len(chunk)
        if total > limit:
            raise PrepareError(message)
        dst.write(chunk)
    return total


def download(url, destination, limit=DOWNLOAD_LIMIT):
    if not url.startswith("https://"):
        raise PrepareError("expected an https archive URL")
    try:
        with urllib.request.urlopen(url, timeout=120) as response, open(destination, "wb") as out:
            declared = response.headers.get("Content-Length")
            if declared and declared.isdigit() and int(declared) > limit:
                raise PrepareError(f"archive declares {declared} bytes, over the {limit} limit")
            return copy_bounded(response, out, limit, f"archive exceeds the {limit} byte download limit")
    except OSError as error:
        raise PrepareError(f"download failed for {url}: {error}") from error


def copy_local(source, destination):
    source = Path(source)
    if not source.is_file():
        raise PrepareError(f"archive not found: {source}")
    try:
        with source.open("rb") as src, open(destination, "wb") as dst:
            copy_bounded(src, dst, DOWNLOAD_LIMIT, f"archive exceeds the {DOWNLOAD_LIMIT} byte limit")
    except OSError as error:
        raise PrepareError(f"cannot read archive {source}: {error}") from error


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(CHUNK), b""):
            digest.update(chunk)
    return digest.hexdigest()


def snapshot_archive(destination, sha256, *, url=None, local=None, prefix=""):
    """Acquire exactly one archive into destination and verify its SHA-256 before any use.

    Pass either an https url (downloaded) or a local path (copied); the copy at destination is what
    gets hashed and returned. Raises PrepareError, leaving no verified snapshot behind, on any refusal.
    """
    if (url is None) == (local is None):
        raise PrepareError("exactly one of a download URL or a local archive is required")
    if url is not None:
        download(url, destination)
        source, kind = url, "download"
    else:
        copy_local(local, destination)
        source, kind = str(local), "archive"
    actual = sha256_file(destination)
    if not hmac.compare_digest(actual, sha256):
        raise PrepareError(f"{prefix}{kind} SHA-256 mismatch for {source}: expected {sha256}, got {actual}")
    return Path(destination)
