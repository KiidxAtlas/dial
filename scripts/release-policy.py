#!/usr/bin/env python3
"""Fail-closed release selection, shared by release automation and offline tests."""
import argparse
import json
import re
from pathlib import Path


def stable_version(tag):
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
    if match is None:
        raise ValueError(f"not a stable release tag: {tag!r}")
    return tuple(map(int, match.groups()))


def is_latest(tag, published):
    candidate = stable_version(tag)
    versions = []
    for other in published:
        # Only versioned stable releases participate; draft/prerelease filtering is
        # performed by the API query before this input is produced.
        if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", other):
            versions.append(stable_version(other))
    return all(candidate >= version for version in versions)


def successful_main_ci(runs, revision):
    return any(
        run.get("event") == "push"
        and run.get("headBranch") == "main"
        and run.get("headSha") == revision
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
        for run in runs
    )

def release_notes(tag, changelog):
    """Select exactly the tagged version's curated section, never Unreleased or other versions."""
    stable_version(tag)
    version = tag[1:]
    headings = list(re.finditer(r"^## \[([^\]\n]+)\][^\n]*$", changelog, re.MULTILINE))
    matches = [index for index, heading in enumerate(headings) if heading.group(1) == version]
    if len(matches) != 1:
        raise ValueError(f"expected exactly one CHANGELOG section for {tag}")
    index = matches[0]
    end = headings[index + 1].start() if index + 1 < len(headings) else len(changelog)
    notes = changelog[headings[index].end():end].strip()
    if not notes:
        raise ValueError(f"CHANGELOG section for {tag} is empty")
    return notes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("latest", "ci", "notes"))
    parser.add_argument("identity")
    parser.add_argument("input", type=Path)
    args = parser.parse_args()
    text = args.input.read_text(encoding="utf-8")
    if args.action == "latest":
        print(f"latest={str(is_latest(args.identity, text.splitlines())).lower()}")
        return 0
    if args.action == "notes":
        try:
            print(release_notes(args.identity, text))
        except ValueError as error:
            parser.error(str(error))
        return 0
    return 0 if successful_main_ci(json.loads(text), args.identity) else 1


if __name__ == "__main__":
    raise SystemExit(main())
