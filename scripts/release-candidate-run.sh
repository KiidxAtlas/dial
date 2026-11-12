#!/usr/bin/env bash
# Print the ID of a successful pull-request CI run whose tested and built source tree is exactly
# the tree of <commit>, so a release can reuse that run's tests and unsigned candidates instead of
# waiting for main's push CI to repeat them. Exits 1, printing nothing, when no such run exists.
#
#   RELEASE_TAG=v0.3.5 scripts/release-candidate-run.sh <commit>
#
# Only pull requests from this repository that were merged into main with a merge commit qualify:
# the head must be an ancestor of <commit>, so the commit reported by `dial --version` is in main's
# history. The run's candidate identity must record <commit>'s tree and the release tag; CI records
# candidates for a pull request only when its tested merge tree equals the head tree that was built.
# Public-key checks remain with the release workflow, which also falls back to building everything.
set -euo pipefail

commit="${1:?usage: RELEASE_TAG=vX.Y.Z $0 <commit>}"
: "${RELEASE_TAG:?RELEASE_TAG is required}"
root="$(git rev-parse --show-toplevel)"
policy="$root/scripts/release-policy.py"
repo="${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
commit="$(git rev-parse "$commit^{commit}")"
tree="$(git rev-parse "$commit^{tree}")"
work="$(mktemp -d "${TMPDIR:-/tmp}/dial-release-candidate.XXXXXX")"
trap 'rm -rf "$work"' EXIT

gh api "repos/$repo/commits/$commit/pulls" > "$work/pulls.json"
python3 "$policy" pr-heads "$repo" "$work/pulls.json" > "$work/heads"
while read -r head; do
  git merge-base --is-ancestor "$head" "$commit" 2>/dev/null || continue
  [[ "$(git rev-parse "$head^{tree}")" == "$tree" ]] || continue
  gh run list -R "$repo" --workflow ci.yml --commit "$head" --event pull_request \
    --json status,conclusion,event,headSha,databaseId > "$work/runs.json"
  run="$(python3 "$policy" pr-run "$head" "$work/runs.json" 2>/dev/null)" || continue
  rm -rf "$work/identity"
  gh run download "$run" -R "$repo" -n release-assets-identity -D "$work/identity" >/dev/null 2>&1 || continue
  python3 "$policy" candidate-tree "$tree" "$work/identity/candidate.json" 2>/dev/null || continue
  echo "$run"
  exit 0
done < "$work/heads"
exit 1
