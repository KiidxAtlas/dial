#!/usr/bin/env python3
"""Print the IDs of superseded Rust caches, one per line, for `gh cache delete`.

    gh cache list --limit 1000 --json id,key,ref,createdAt > caches.json
    python3 scripts/prune-caches.py caches.json

Swatinem/rust-cache keys end in a hash of the lockfiles and toolchain, so every dependency or
toolchain change leaves the previous entry behind until GitHub evicts it. Stale entries fill the
repository's cache quota and push out the release-candidate caches, so every release build
starts cold. Per ref and key prefix (the key without its final hash), only the newest entry is
kept. Other caches (Docker layers, CodeQL) are never selected.
"""
import json
import sys


def superseded(caches):
    newest = {}
    stale = []
    for cache in sorted(caches, key=lambda c: c["createdAt"], reverse=True):
        key = cache.get("key", "")
        if not key.startswith("v0-rust-") or "-" not in key:
            continue
        group = (cache.get("ref"), key.rsplit("-", 1)[0])
        if group in newest:
            stale.append(cache["id"])
        else:
            newest[group] = cache["id"]
    return stale


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    with open(sys.argv[1], encoding="utf-8") as handle:
        caches = json.load(handle)
    for cache_id in superseded(caches):
        print(cache_id)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
