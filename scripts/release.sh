#!/usr/bin/env bash
# Prepare a Dial release commit and tag. It never pushes unless --push is given.
#
#   scripts/release.sh 0.1.23            # bump, roll the changelog, check, commit, tag
#   scripts/release.sh 0.1.23 --push     # ...then push main and the tag (starts release.yml)
#   scripts/release.sh 0.1.23 --no-check # skip local fmt/clippy/tests (CI still gates publishing)
#
# Steps: require a clean checkout on main; set [workspace.package] version in Cargo.toml; refresh
# Cargo.lock for workspace members only; move `## [Unreleased]` entries under `## [<version>] -
# <date>` in CHANGELOG.md; run the local gate; commit `release: v<version>`; tag `v<version>`.
set -euo pipefail

usage() { sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

version="${1:-}"
[[ -n "$version" ]] || usage
shift
push=0
check=1
for arg in "$@"; do
  case "$arg" in
    --push) push=1 ;;
    --no-check) check=0 ;;
    *) usage ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 1.2.3, got: $version" >&2; exit 2; }
tag="v$version"

cd "$(git rev-parse --show-toplevel)"

branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$branch" == main ]] || { echo "releases are cut from main (current branch: $branch)" >&2; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is not clean; commit or stash your changes first" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "tag $tag already exists" >&2
  exit 1
fi

current="$(python3 - <<'PY'
import re, pathlib
text = pathlib.Path('Cargo.toml').read_text()
section = re.search(r'^\[workspace\.package\]\n(.*?)(?=^\[)', text, re.M | re.S)
match = section and re.search(r'^version = "([^"]+)"', section.group(1), re.M)
if not match:
    raise SystemExit('could not find [workspace.package] version in Cargo.toml')
print(match.group(1))
PY
)"
[[ "$current" != "$version" ]] || { echo "Cargo.toml is already at $version" >&2; exit 1; }
echo "Releasing $current -> $version"

RELEASE_VERSION="$version" python3 - <<'PY'
import datetime, os, pathlib, re

version = os.environ['RELEASE_VERSION']

cargo = pathlib.Path('Cargo.toml')
text = cargo.read_text()
section = re.search(r'^\[workspace\.package\]\n(.*?)(?=^\[)', text, re.M | re.S)
body = re.sub(r'^version = "[^"]+"', f'version = "{version}"', section.group(1), count=1, flags=re.M)
cargo.write_text(text[:section.start(1)] + body + text[section.end(1):])

changelog = pathlib.Path('CHANGELOG.md')
log = changelog.read_text()
heading = '## [Unreleased]\n'
if heading not in log:
    raise SystemExit('CHANGELOG.md has no "## [Unreleased]" section')
start = log.index(heading) + len(heading)
rest = log[start:]
nxt = re.search(r'^## \[', rest, re.M)
entries = rest[:nxt.start()] if nxt else rest
if not entries.strip():
    raise SystemExit('the [Unreleased] section of CHANGELOG.md is empty; describe the release first')
date = datetime.date.today().isoformat()
changelog.write_text(log[:start] + f'\n## [{version}] - {date}\n' + rest)
PY

# Workspace members only: never upgrades third-party dependencies as a side effect.
cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace

if [[ "$check" == 1 ]]; then
  cargo fmt --check
  make lint
  make test-rust
fi

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: $tag"
git tag -a "$tag" -m "Dial $tag"
echo "Created commit and tag $tag."

if [[ "$push" == 1 ]]; then
  git push origin main
  git push origin "$tag"
  echo "Pushed. release.yml waits for CI on this commit before publishing."
else
  echo "Next: git push origin main && git push origin $tag"
fi
