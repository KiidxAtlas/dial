#!/bin/sh
# Run a Harbor native checkout build with the pinned CPython (no network).
# Run from the checkout root; prefetch the matching upstream archive listed in
# runtimes/python/manifest.json to target/python-archives/<triple>.tar.gz.
set -eu
version=$(rustc -vV)
host=$(printf '%s\n' "$version" | sed -n 's/^host: //p')
test -n "$host" || { echo 'rustc reported no host target' >&2; exit 1; }
archive=${DIAL_PYTHON_ARCHIVE:-target/python-archives/$host.tar.gz}
if ! test -f "$archive"; then
    echo "missing pinned CPython archive for $host at $archive; prefetch before the offline build" >&2
    exit 1
fi
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
export DIAL_BUNDLED_PYTHON_DIR="$scratch/payload"
python3 scripts/prepare-python.py --archive "$archive" --target "$host" --output "$DIAL_BUNDLED_PYTHON_DIR"
"$@"
