#!/usr/bin/env python3
"""Stage the pinned, relocatable CPython distribution for an offline Cargo build.

Reads an archive from --archive (no network) or the per-target HTTPS manifest URL.
Nothing is installed. Cargo receives the result through DIAL_BUNDLED_PYTHON_DIR.
"""
import argparse
import gzip
import io
import json
import posixpath
import re
import shutil
import tarfile
import tempfile
from pathlib import Path

from runtime_payload import CHUNK, DOWNLOAD_LIMIT, PrepareError, download, sha256_file

ROOT = Path(__file__).resolve().parent.parent / "runtimes" / "python"
LIMIT = 256 * 1024 * 1024
NOTICES = "LICENSE.build-standalone"


def relative(name):
    if not name or any(p in {"", ".", ".."} for p in name.split("/")) or any(c in name for c in "\\:\x00"):
        raise PrepareError(f"unsafe archive path {name!r}")
    return name


def add(out, name, size, mode, stream):
    info = tarfile.TarInfo(name)  # fixed ownership and epoch; do not copy special permissions
    info.size = size
    info.mode = 0o755 if mode & 0o111 else 0o644
    out.addfile(info, stream)


def normalize(source, raw, target, version, notices):
    """Rebuild only regular files, resolving upstream links inside the verified archive, never via the host FS."""
    with tarfile.open(source, "r:") as src, tarfile.open(raw, "w", format=tarfile.USTAR_FORMAT) as out:
        entries = {}
        for m in src:
            if m.name == "python" and m.isdir():
                continue
            if not m.name.startswith("python/"):
                raise PrepareError(f"unexpected distribution member {m.name!r}")
            name = relative(m.name[len("python/"):])
            if name in entries or len(entries) >= 32767 or not (m.isfile() or m.isdir() or m.issym()):
                raise PrepareError(f"duplicate, excessive or special archive member {name!r}")
            entries[name] = m
        windows = "windows" in target
        binary = "python.exe" if windows else "bin/python3"
        license_name = "LICENSE.txt" if windows else f"lib/python{'.'.join(version.split('.')[:2])}/LICENSE.txt"
        if binary not in entries or license_name not in entries or NOTICES in entries:
            raise PrepareError("distribution lacks interpreter/license or collides with upstream notices")
        license_text = None
        total = len(notices)
        for name, m in sorted(entries.items()):
            if m.isdir():
                if name == binary:
                    raise PrepareError("interpreter entry is a directory")
                continue
            seen = set()
            while m.issym():
                if m.name in seen:
                    raise PrepareError(f"cyclic link {name!r}")
                seen.add(m.name)
                dest = posixpath.normpath(posixpath.join(posixpath.dirname(m.name), m.linkname))
                m = entries.get(dest.removeprefix("python/")) if dest.startswith("python/") else None
                if m is None:
                    raise PrepareError(f"escaping or dangling link {name!r}")
            if not m.isfile():
                raise PrepareError(f"expected regular file at {name!r}")
            if name == binary and (not m.size or (not windows and not m.mode & 0o111)):
                raise PrepareError("interpreter is empty or not executable")
            total += m.size + 1024
            if total > LIMIT:
                raise PrepareError("normalized payload exceeds size limit")
            with src.extractfile(m) as content:
                if name == license_name:
                    if not 0 < m.size < 1024 * 1024:
                        raise PrepareError("invalid CPython license size")
                    license_text = content.read()
                    content.seek(0)
                add(out, name, m.size, m.mode, content)
        if not license_text or not license_text.strip():
            raise PrepareError("missing CPython license text")
        add(out, NOTICES, len(notices), 0o644, io.BytesIO(notices))
    return license_text + b"\n\n" + notices


def prepare(target, output, archive=None, manifest=ROOT / "manifest.json"):
    data = json.loads(manifest.read_text(encoding="utf-8"))
    if target not in data["targets"]:
        raise PrepareError(f"no pinned Python for target {target}")
    entry = data["targets"][target]
    version = data["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version) or not re.fullmatch(r"[a-f0-9]{64}", entry["sha256"]):
        raise PrepareError("invalid Python version or archive SHA-256 in manifest")
    if output.exists():
        raise PrepareError(f"output already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output.parent, prefix=".prepare-python-") as tmp:
        tmp = Path(tmp)
        if archive is None:
            archive = tmp / "download.tar.gz"
            download(entry["url"], archive)
        # Snapshot even a local archive before verifying: do not reopen a caller-owned path for extraction.
        else:
            snapshot = tmp / "local.tar.gz"
            with archive.open("rb") as src, snapshot.open("wb") as dst:
                copy_bounded(src, dst, DOWNLOAD_LIMIT)
            archive = snapshot
        if sha256_file(archive) != entry["sha256"]:
            raise PrepareError(f"Python archive SHA-256 mismatch for {target}")
        source = tmp / "source.tar"
        with gzip.open(archive, "rb") as src, source.open("wb") as dst:
            copy_bounded(src, dst, LIMIT)
        raw = tmp / "normalized.tar"
        licenses = normalize(source, raw, target, version, (ROOT / NOTICES).read_bytes())
        size = raw.stat().st_size
        if size > LIMIT:
            raise PrepareError("normalized tar exceeds size limit")
        staging = tmp / "payload"
        staging.mkdir()
        with (staging / "python.tar.gz").open("wb") as dst, raw.open("rb") as src:
            with gzip.GzipFile(filename="", mode="wb", fileobj=dst, mtime=0, compresslevel=6) as stream:
                shutil.copyfileobj(src, stream, CHUNK)
        (staging / "metadata.txt").write_bytes(f"{target}\n{version}\n{sha256_file(raw)}\n{size}\n".encode("ascii"))
        (staging / "LICENSE.python").write_bytes(licenses)
        staging.rename(output)


def copy_bounded(src, dst, limit):
    total = 0
    while chunk := src.read(CHUNK):
        total += len(chunk)
        if total > limit:
            raise PrepareError(f"Python archive exceeds the {limit} byte limit")
        dst.write(chunk)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--archive", type=Path, help="verified local archive instead of a download")
    args = parser.parse_args()
    try:
        prepare(args.target, args.output, args.archive)
    except (ValueError, OSError, EOFError, tarfile.TarError) as error:
        parser.exit(1, f"prepare-python: {error}\n")
    print(f"prepared bundled CPython in {args.output}")


if __name__ == "__main__":
    main()
