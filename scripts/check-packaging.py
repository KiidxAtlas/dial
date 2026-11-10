#!/usr/bin/env python3
"""Check that source-built package metadata agrees with the Cargo workspace version.

- `flake.nix` must derive `version` from `Cargo.toml` (`[workspace.package]`) rather than hard-coding a literal.
- `Formula/dial.rb` is release-fed: it may lag unreleased source, so its version is not compared
  with Cargo. It must still be self-consistent: every archive URL is versioned by `#{version}`
  and each URL is followed by a sha256 line.
- `CHANGELOG.md` release headings must have unique references; Unreleased compares the latest release to HEAD.

Exit status is non-zero and each problem is printed when a check fails.
"""
import re
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FLAKE_DERIVED = re.compile(
    r"version\s*=\s*\(builtins\.fromTOML\s*\(builtins\.readFile\s+\./Cargo\.toml\)\)\.workspace\.package\.version\s*;"
)
FLAKE_LITERAL = re.compile(r'^\s*version\s*=\s*"[^"]*"\s*;', re.MULTILINE)
FORMULA_VERSION = re.compile(r'^  version "([^"]+)"$', re.MULTILINE)
FORMULA_URL = re.compile(r'^\s*url "([^"]+)"$', re.MULTILINE)
FORMULA_SHA = re.compile(r'^\s*sha256 (?::no_check|"[^"]*")$', re.MULTILINE)
CHANGELOG_REPOSITORY = "https://github.com/KiidxAtlas/dial"
CHANGELOG_VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def changelog_metadata(text):
    """Parse the release headings and their reference definitions, refusing ambiguity."""
    headings = re.findall(r"^## \[([^\]\n]+)\](?: - \d{4}-\d{2}-\d{2})?[ \t]*$", text, re.M)
    if (not headings or headings[0] != "Unreleased" or len(headings) < 2
            or len(headings) != len(set(headings))
            or any(not re.fullmatch(CHANGELOG_VERSION, v) for v in headings[1:])
            or len(re.findall(r"^## ", text, re.M)) != len(headings)):
        raise ValueError("CHANGELOG.md needs one leading [Unreleased] and unique stable release headings")
    refs = {}
    for match in re.finditer(r"^\[([^\]\n]+)\]:[^\n]*$", text, re.M):
        label = match.group(1)
        if label.lower() == "unreleased" or re.fullmatch(CHANGELOG_VERSION, label):
            key = "Unreleased" if label.lower() == "unreleased" else label
            if key in refs:
                raise ValueError(f"CHANGELOG.md has duplicate [{key}] references")
            refs[key] = match
    return headings[1:], refs


def changelog_problems(text):
    try:
        versions, refs = changelog_metadata(text)
    except ValueError as error:
        return [str(error)]
    problems = []
    for label in ["Unreleased", *versions]:
        if label not in refs:
            problems.append(f"CHANGELOG.md is missing [{label}] reference")
            continue
        url = refs[label].group(0).split(":", 1)[1].strip()
        if label == "Unreleased":
            valid = url == f"{CHANGELOG_REPOSITORY}/compare/v{versions[0]}...HEAD"
        else:
            valid = (url == f"{CHANGELOG_REPOSITORY}/tree/v{label}"
                     or re.fullmatch(re.escape(CHANGELOG_REPOSITORY) + r"/commit/[0-9a-f]{7,40}", url))
        if not valid:
            problems.append(f"CHANGELOG.md has a stale or invalid [{label}] reference")
    return problems


def update_changelog_references(text):
    """Repair the moving comparison and absent release links, retaining historical commit links."""
    versions, refs = changelog_metadata(text)
    unreleased = f"[Unreleased]: {CHANGELOG_REPOSITORY}/compare/v{versions[0]}...HEAD"
    if "Unreleased" in refs:
        match = refs["Unreleased"]
        text = text[:match.start()] + unreleased + text[match.end():]
    else:
        text = text.rstrip() + "\n\n" + unreleased + "\n"
    missing = [f"[{v}]: {CHANGELOG_REPOSITORY}/tree/v{v}" for v in versions if v not in refs]
    if missing:
        text = text.rstrip() + "\n" + "\n".join(missing) + "\n"
    problems = changelog_problems(text)
    if problems:
        raise ValueError("; ".join(problems))
    return text


def cargo_version(cargo_text):
    """Version of the `[workspace.package]` table in Cargo.toml text; every member inherits it."""
    return tomllib.loads(cargo_text)["workspace"]["package"]["version"]


def flake_problems(flake_text):
    problems = []
    if not FLAKE_DERIVED.search(flake_text):
        problems.append("flake.nix must derive version from ./Cargo.toml "
                        "(builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version")
    if FLAKE_LITERAL.search(flake_text):
        problems.append("flake.nix hard-codes a version literal; derive it from Cargo.toml")
    return problems


def formula_problems(formula_text):
    problems = []
    if len(FORMULA_VERSION.findall(formula_text)) != 1:
        problems.append("Formula/dial.rb must declare exactly one `  version \"...\"` line")
    urls = FORMULA_URL.findall(formula_text)
    if not urls:
        problems.append("Formula/dial.rb declares no archive urls")
    for url in urls:
        if "/v#{version}/" not in url or "dial-v#{version}-" not in url:
            problems.append(f"Formula url is not versioned by #{{version}}: {url}")
    if len(FORMULA_SHA.findall(formula_text)) != len(urls):
        problems.append("every Formula url must be followed by exactly one sha256 line")
    return problems


def check(root=ROOT):
    cargo = cargo_version((root / "Cargo.toml").read_text(encoding="utf-8"))
    problems = flake_problems((root / "flake.nix").read_text(encoding="utf-8"))
    problems += formula_problems((root / "Formula" / "dial.rb").read_text(encoding="utf-8"))
    problems += changelog_problems((root / "CHANGELOG.md").read_text(encoding="utf-8"))
    return cargo, problems


def main():
    cargo, problems = check()
    for problem in problems:
        print(f"packaging: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"packaging consistent for Cargo version {cargo}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
