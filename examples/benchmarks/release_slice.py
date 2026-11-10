#!/usr/bin/env python3
"""Validate the release slice and print the commands that would run it.

This never invokes Dial, a model or a provider. It checks that release-slice.toml still matches the
committed suites and profiles, then prints one `dial benchmark` command and one `reliability.py`
command per suite for a person to run."""
import argparse
import shlex
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent
MANIFEST = ROOT / "release-slice.toml"
MINIMUM_REPETITIONS = 3
PLACEHOLDER_MODEL = "REPLACE_WITH_LOADED_MODEL_ID"


def load(path=MANIFEST):
    """Return the validated manifest; raise ValueError naming the first mismatch."""
    manifest = tomllib.loads(path.read_text())
    if manifest.get("version") != 1:
        raise ValueError(f"{path.name}: unsupported version {manifest.get('version')!r}")
    if not str(manifest.get("name", "")).strip():
        raise ValueError(f"{path.name}: name is required")
    repetitions = manifest.get("repetitions")
    if not isinstance(repetitions, int) or not MINIMUM_REPETITIONS <= repetitions <= 10:
        raise ValueError(f"{path.name}: repetitions must be {MINIMUM_REPETITIONS}..=10, not {repetitions!r}")
    suites = manifest.get("suites") or []
    if not suites:
        raise ValueError(f"{path.name}: at least one suite is required")
    seen = set()
    for suite in suites:
        name = suite["path"]
        if name in seen:
            raise ValueError(f"{path.name}: {name} is listed twice")
        seen.add(name)
        if Path(name).name != name:
            raise ValueError(f"{path.name}: {name} must be a file beside the manifest")
        actual = len(tomllib.loads((path.parent / name).read_text()).get("cases", []))
        if actual != suite["cases"]:
            raise ValueError(
                f"{path.name}: {name} has {actual} cases but the slice pins {suite['cases']}; "
                "revise the slice name with the count"
            )
    for label, remote in (("hosted", True), ("local", False)):
        name = manifest.get("profiles", {}).get(label)
        if not name:
            raise ValueError(f"{path.name}: profiles.{label} is required")
        machine = tomllib.loads((path.parent / name).read_text())["machine"]
        if machine["repetitions"] != repetitions:
            raise ValueError(f"{name}: repetitions {machine['repetitions']} differ from the slice's {repetitions}")
        if machine["allow_remote"] is not remote:
            raise ValueError(f"{name}: allow_remote must be {str(remote).lower()} for the {label} profile")
    return manifest


def trials(manifest, configurations=1):
    """Case runs the slice asks for: cases x repetitions x configurations."""
    return sum(suite["cases"] for suite in manifest["suites"]) * manifest["repetitions"] * configurations


def plan(manifest, profile, label, dial="dial"):
    """Commands for one profile, in suite order. Reports go under benchmarks/release-slice/<label>/."""
    profiles = Path("examples/benchmarks") / manifest["profiles"][profile]
    out = Path("benchmarks/release-slice") / label
    commands = [["mkdir", "-p", str(out)]]
    for suite in manifest["suites"]:
        stem = Path(suite["path"]).stem
        report = out / f"{stem}.json"
        commands.append([
            dial, "benchmark", str(Path("examples/benchmarks") / suite["path"]),
            "--profiles", str(profiles), "--report", str(report),
        ])
        commands.append([
            "python3", "examples/benchmarks/reliability.py", str(report),
            "--output", str(out / f"{stem}-reliability.json"),
        ])
    return commands


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=("hosted", "local"), default="local")
    parser.add_argument("--label", default="candidate", help="report directory name, e.g. a version or commit")
    parser.add_argument("--dial", default="dial", help="the Dial binary under test")
    args = parser.parse_args()
    if Path(args.label).name != args.label or args.label in ("", ".", ".."):
        parser.error("--label must be a plain directory name")
    try:
        manifest = load()
    except (ValueError, KeyError, OSError, tomllib.TOMLDecodeError) as error:
        parser.error(str(error))
    cases = sum(suite["cases"] for suite in manifest["suites"])
    print(f"# {manifest['name']}: {cases} cases x {manifest['repetitions']} repetitions = {trials(manifest)} case runs")
    if args.profile == "hosted":
        print("# These commands send prompts to a hosted provider and spend its quota. Nothing has been run.")
    else:
        print(f"# Replace {PLACEHOLDER_MODEL} in the local profile first. Nothing has been run.")
    print("# Run from the repository root. Existing reliability summaries are never overwritten.")
    for command in plan(manifest, args.profile, args.label, args.dial):
        print(shlex.join(command))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
