#!/usr/bin/env bash
# Prepare release metadata without building, staging, committing or tagging.
#
#   scripts/release.sh 0.3.0             # prepare only; safe to repeat
#   scripts/release.sh 0.3.0 --tag       # tag an already-prepared, clean, CI-tested main commit
#   scripts/release.sh 0.3.0 --tag --push # ...then push the tag (starts release.yml)
# Commit and push the prepared source separately, only after review and authorization.
set -euo pipefail

usage() { sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
version="${1:-}"
[[ -n "$version" ]] || usage
shift
tag_only=0
push=0
for arg in "$@"; do
  case "$arg" in
    --prepare) ;;
    --tag) tag_only=1 ;;
    --push) push=1 ;;
    *) usage ;;
  esac
done
[[ "$push" == 0 || "$tag_only" == 1 ]] || { echo '--push requires --tag; preparation never pushes' >&2; exit 2; }
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || { echo 'version must be a stable version such as 0.3.0' >&2; exit 2; }
tag="v$version"
cd "$(git rev-parse --show-toplevel)"

if [[ "$tag_only" == 0 ]]; then
  # Validate everything that can refuse before changing either source file.
  RELEASE_VERSION="$version" python3 - <<'PY'
import datetime, os, pathlib, re
version = os.environ['RELEASE_VERSION']
cargo = pathlib.Path('Cargo.toml')
text = cargo.read_text()
section = re.search(r'^\[workspace\.package\]\n(.*?)(?=^\[|\Z)', text, re.M | re.S)
match = section and re.search(r'^version = "([^"]+)"', section.group(1), re.M)
if not match:
    raise SystemExit('could not find [workspace.package] version in Cargo.toml')
changelog = pathlib.Path('CHANGELOG.md')
log = changelog.read_text()
heading = '## [Unreleased]\n'
if log.count(heading) != 1:
    raise SystemExit('CHANGELOG.md must have exactly one [Unreleased] section')
if match.group(1) == version:
    print(f'Already prepared at {version}; checking existing release metadata.')
else:
    if re.search(r'^## \[' + re.escape(version) + r'\]', log, re.M):
        raise SystemExit(f'CHANGELOG.md already contains {version}; review it before preparing')
    start = log.index(heading) + len(heading)
    rest = log[start:]
    nxt = re.search(r'^## \[', rest, re.M)
    entries = rest[:nxt.start()] if nxt else rest
    if not entries.strip():
        raise SystemExit('the [Unreleased] section is empty; describe the release first')
    body = re.sub(r'^version = "[^"]+"', f'version = "{version}"', section.group(1), count=1, flags=re.M)
    cargo.write_text(text[:section.start(1)] + body + text[section.end(1):])
    date = datetime.date.today().isoformat()
    changelog.write_text(log[:start] + f'\n## [{version}] - {date}\n' + rest)
PY
  # Resolve workspace versions only. This does not compile, upgrade external dependencies,
  # or silently fall back to the network. On failure keep the edits for inspection/retry.
  cargo update --workspace --offline
  python3 scripts/release-policy.py check "$tag"
  echo "Prepared $tag. Nothing was staged, committed, tagged or pushed."
  echo 'Next: make release-check; review and commit separately; push main and wait for CI before --tag.'
  exit 0
fi

[[ "$(git rev-parse --abbrev-ref HEAD)" == main ]] || { echo 'tags are cut from main' >&2; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo 'working tree is not clean; review and commit the prepared changes first' >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "tag $tag already exists" >&2
  exit 1
fi
make release-check RELEASE_TAG="$tag"
command -v gh >/dev/null 2>&1 || { echo 'gh is required to confirm exact-commit main CI' >&2; exit 1; }
git fetch -q origin main
head="$(git rev-parse HEAD)"
[[ "$head" == "$(git rev-parse origin/main)" ]] || { echo 'HEAD must match origin/main; push the reviewed commit and let CI pass first' >&2; exit 1; }
runs="$(mktemp "${TMPDIR:-/tmp}/dial-release-ci.XXXXXX")"
trap 'rm -f "$runs"' EXIT
gh run list --workflow ci.yml --commit "$head" --event push --branch main --json status,conclusion,event,headBranch,headSha > "$runs"
if ! python3 scripts/release-policy.py ci "$head" "$runs"; then
  echo "no successful main push CI for $head; no tag created" >&2
  id="$(gh run list --workflow ci.yml --commit "$head" --event push --branch main --limit 1 --json databaseId --jq '.[0].databaseId // empty')"
  if [[ -n "$id" ]]; then
    gh run view "$id" --json jobs --jq '.jobs[] | select(.conclusion == "failure" or .conclusion == "cancelled" or .conclusion == "timed_out") | "  failed: \(.name) (\(.conclusion)) \(.url)"' >&2 || true
  fi
  exit 1
fi
git tag -a "$tag" -m "Dial $tag"
echo "Created annotated tag $tag on the prepared, CI-tested commit."
if [[ "$push" == 1 ]]; then
  git push origin "$tag"
else
  echo "Next, only when authorized: git push origin $tag"
fi
