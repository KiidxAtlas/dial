#!/usr/bin/env python3
"""Start the next release after publishing one (the release train).

    python3 scripts/advance-release.py --released 0.3.9 [--date 2026-11-16] [--root .]

- Dates the released notes: when CHANGELOG.md has no section for the released version, the
  Unreleased notes become `## [<released>] - <date>` and Unreleased starts empty.
- Moves the workspace to the next patch version: when Cargo.toml still has the released version,
  it and every local Cargo.lock entry at that version move to <released> with the patch + 1.

Main then always carries an untagged version, so CI builds release candidates on every main
commit and a release is only a tag. Repeating the command changes nothing. It never runs Cargo,
Git or the network; the result must pass `scripts/release-policy.py check`.
"""
import argparse
import datetime
import importlib.util
import re
import sys
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


packaging = load("check_packaging", "check-packaging.py")
policy = load("release_policy", "release-policy.py")


def next_patch(version):
    major, minor, patch = policy.stable_version(f"v{version}")
    return f"{major}.{minor}.{patch + 1}"


def date_changelog(text, released, date):
    """Roll the Unreleased notes into the released version's dated section, once."""
    if re.search(r"^## \[" + re.escape(released) + r"\]", text, re.M):
        return packaging.update_changelog_references(text)
    heading = "## [Unreleased]\n"
    if text.count(heading) != 1:
        raise ValueError("CHANGELOG.md must have exactly one [Unreleased] section")
    # The published notes came from Unreleased; refuse to date an empty section.
    policy.release_notes(f"v{released}", text)
    start = text.index(heading) + len(heading)
    text = text[:start] + f"\n## [{released}] - {date}\n" + text[start:]
    return packaging.update_changelog_references(text)


def bump_manifest(text, released, following):
    section = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", text, re.M | re.S)
    match = section and re.search(r'^version = "([^"]+)"', section.group(1), re.M)
    if not match:
        raise ValueError("could not find [workspace.package] version in Cargo.toml")
    if match.group(1) != released:
        return text, False
    body = re.sub(r'^version = "[^"]+"', f'version = "{following}"', section.group(1), count=1, flags=re.M)
    return text[:section.start(1)] + body + text[section.end(1):], True


def bump_lock(text, released, following):
    """Move local (sourceless) lock entries at the released version; registry entries keep theirs."""
    blocks = text.split("\n[[package]]\n")
    for index in range(1, len(blocks)):
        block = blocks[index]
        if re.search(r"^source = ", block, re.M):
            continue
        blocks[index] = re.sub(r'^version = "' + re.escape(released) + r'"$', f'version = "{following}"',
                               block, count=1, flags=re.M)
    return "\n[[package]]\n".join(blocks)


def advance(root, released, date):
    policy.stable_version(f"v{released}")
    changelog = root / "CHANGELOG.md"
    cargo = root / "Cargo.toml"
    lock = root / "Cargo.lock"
    following = next_patch(released)
    log = date_changelog(changelog.read_text(encoding="utf-8"), released, date)
    manifest, bumped = bump_manifest(cargo.read_text(encoding="utf-8"), released, following)
    locked = bump_lock(lock.read_text(encoding="utf-8"), released, following) if bumped else lock.read_text(encoding="utf-8")
    changelog.write_text(log, encoding="utf-8")
    cargo.write_text(manifest, encoding="utf-8")
    lock.write_text(locked, encoding="utf-8")
    problems = policy.release_problems(root)
    if problems:
        raise ValueError("; ".join(problems))
    return following if bumped else None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--released", required=True, help="the version just published, such as 0.3.9")
    parser.add_argument("--date", default=datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    parser.add_argument("--root", type=Path, default=SCRIPTS.parent)
    args = parser.parse_args()
    try:
        following = advance(args.root, args.released, args.date)
    except (OSError, ValueError) as error:
        print(f"advance-release: {error}", file=sys.stderr)
        return 1
    print(f"Released {args.released} is dated; workspace now at {following}." if following
          else f"Released {args.released} is dated; workspace version unchanged.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
