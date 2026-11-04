#!/usr/bin/env python3
"""Fail-closed release selection, shared by release automation and offline tests."""
import argparse
import json
import os
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
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


def release_problems(root, tag=None):
    """Cheap source checks only: never invoke Cargo, a compiler, or the network."""
    problems = []
    try:
        manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
        workspace = manifest["workspace"]
        version = workspace["package"]["version"]
        stable_version(f"v{version}")
    except (OSError, ValueError, KeyError, TypeError) as error:
        return [f"Cargo.toml: {error}"]
    if tag is not None:
        try:
            stable_version(tag)
        except ValueError as error:
            problems.append(str(error))
        if tag != f"v{version}":
            problems.append(f"tag {tag} must match Cargo.toml version v{version}")
    members = {root / "Cargo.toml"}
    for pattern in workspace["members"]:
        matches = [root] if pattern == "." else [path for path in root.glob(pattern) if path.is_dir()]
        if not matches:
            problems.append(f"workspace member pattern has no matches: {pattern}")
        members.update(path / "Cargo.toml" for path in matches)
    for pattern in workspace.get("exclude", []):
        matches = [root] if pattern == "." else list(root.glob(pattern))
        members.difference_update(path / "Cargo.toml" for path in matches)
    names = set()
    for path in sorted(members):
        try:
            package = tomllib.loads(path.read_text(encoding="utf-8"))["package"]
            name = package["name"]
            if name in names:
                problems.append(f"duplicate workspace package: {name}")
            names.add(name)
            if package.get("version") != {"workspace": True}:
                problems.append(f"{path.relative_to(root)} must inherit workspace version")
        except (OSError, ValueError, KeyError, TypeError) as error:
            problems.append(f"{path.relative_to(root)}: {error}")
    try:
        packages = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))["package"]
        for name in sorted(names):
            entries = [p for p in packages if p["name"] == name and "source" not in p]
            if len(entries) != 1 or entries[0].get("version") != version:
                problems.append(f"Cargo.lock: {name} must have one local entry at {version}")
    except (OSError, ValueError, KeyError, TypeError) as error:
        problems.append(f"Cargo.lock: {error}")
    try:
        release_notes(f"v{version}", (root / "CHANGELOG.md").read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        problems.append(f"CHANGELOG.md: {error}")
    return problems


def key_problems(require_private=False):
    """Validate environment inputs without printing any key material."""
    from base64 import b64decode
    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey, Ed25519PrivateKey
    except ImportError:
        return ["signing readiness needs Python cryptography (scripts/requirements-ci.lock)"]
    problems = []
    raw = public = private = None
    try:
        raw = b64decode(os.environ.get("DIAL_RELEASE_PUBLIC_KEY", ""), validate=True)
        if len(raw) != 32:
            raise ValueError()
    except ValueError:
        problems.append("DIAL_RELEASE_PUBLIC_KEY must encode exactly 32 bytes as base64")
    try:
        public = serialization.load_pem_public_key(os.environ.get("DIAL_RELEASE_PUBLIC_KEY_PEM", "").encode())
        if not isinstance(public, Ed25519PublicKey):
            raise ValueError()
        if public.public_bytes_raw() != raw:
            problems.append("public PEM and raw public key do not match")
    except (ValueError, TypeError):
        problems.append("DIAL_RELEASE_PUBLIC_KEY_PEM must be an Ed25519 public PEM")
    if require_private:
        try:
            private = serialization.load_pem_private_key(os.environ.get("DIAL_RELEASE_SIGNING_KEY", "").encode(), password=None)
            if not isinstance(private, Ed25519PrivateKey):
                raise ValueError()
            if private.public_key().public_bytes_raw() != raw:
                problems.append("signing key does not match the configured public key")
        except (ValueError, TypeError):
            problems.append("DIAL_RELEASE_SIGNING_KEY must be an unencrypted Ed25519 private PEM")
    return problems


def tool_problems():
    import importlib.util
    import shutil
    problems = []
    for command in ("bash", "make", "node", "cargo", "rustup"):
        if shutil.which(command) is None:
            problems.append(f"missing command: {command}")
    for module in ("cryptography", "pytest"):
        if importlib.util.find_spec(module) is None:
            problems.append(f"missing Python {module}; use Python 3.12 with scripts/requirements-ci.lock")
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("latest", "ci", "notes", "check", "keys", "tools"))
    parser.add_argument("identity", nargs="?")
    parser.add_argument("input", type=Path, nargs="?")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--private", action="store_true", help="also validate the signing key; keys action only")
    args = parser.parse_args()
    if args.private and args.action != "keys":
        parser.error("--private requires the keys action")
    if args.action in ("check", "keys", "tools"):
        if args.action == "check":
            problems = release_problems(args.root, args.identity)
        elif args.action == "keys":
            problems = key_problems(args.private)
        else:
            problems = tool_problems()
        for problem in problems:
            print(f"release: {problem}", file=sys.stderr)
        if not problems:
            print(f"release {args.action}: passed (no compilation)")
        return int(bool(problems))
    if args.identity is None or args.input is None:
        parser.error(f"{args.action} requires an identity and input file")
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
