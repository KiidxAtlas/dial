"""Bounded I/O for pinned runtime payloads (build time only)."""
import hashlib
import urllib.request

CHUNK = 1024 * 1024
DOWNLOAD_LIMIT = 150 * 1024 * 1024


class PrepareError(ValueError):
    """A refusal with a message suitable for the build log."""


def download(url, destination, limit=DOWNLOAD_LIMIT):
    if not url.startswith("https://"):
        raise PrepareError("expected an https archive URL")
    total = 0
    try:
        with urllib.request.urlopen(url, timeout=120) as response, open(destination, "wb") as out:
            declared = response.headers.get("Content-Length")
            if declared and declared.isdigit() and int(declared) > limit:
                raise PrepareError(f"archive declares {declared} bytes, over the {limit} limit")
            while chunk := response.read(CHUNK):
                total += len(chunk)
                if total > limit:
                    raise PrepareError(f"archive exceeds the {limit} byte download limit")
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
