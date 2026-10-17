#!/usr/bin/env python3
"""Check that source-built package metadata agrees with the Cargo workspace version.

- `flake.nix` must derive `version` from `Cargo.toml` (`[workspace.package]`) rather than hard-coding a literal.
- `Formula/dial.rb` is release-fed: it may lag unreleased source, so its version is not compared
  with Cargo. It must still be self-consistent: every archive URL is versioned by `#{version}`
  and each URL is followed by a sha256 line.

Exit status is non-zero and each problem is printed when a check fails.
"""
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FLAKE_DERIVED = re.compile(
    r"version\s*=\s*\(builtins\.fromTOML\s*\(builtins\.readFile\s+\./Cargo\.toml\)\)\.workspace\.package\.version\s*;"
)
FLAKE_LITERAL = re.compile(r'^\s*version\s*=\s*"[^"]*"\s*;', re.MULTILINE)
FORMULA_VERSION = re.compile(r'^  version "([^"]+)"$', re.MULTILINE)
FORMULA_URL = re.compile(r'^\s*url "([^"]+)"$', re.MULTILINE)
FORMULA_SHA = re.compile(r'^\s*sha256 (?::no_check|"[^"]*")$', re.MULTILINE)


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
