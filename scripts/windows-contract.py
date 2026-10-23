#!/usr/bin/env python3
"""Run the focused Windows Rust contract listed in scripts/windows-contract.txt.

Each `test` line selects workspace libraries; `test-bin` selects a package binary.
Every `skip` line is passed to libtest as `--skip` and printed with its reason.
Successful Cargo exits with no executed tests are rejected, including ignored-only selections.
`--print` shows the commands without running them (it does not verify selection).
"""
import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONTRACT = ROOT / "scripts" / "windows-contract.txt"


def parse(path):
    tests, skips = [], []
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        kind, _, rest = line.partition(" ")
        name, _, reason = rest.partition(" # ")
        name, reason = name.strip(), reason.strip()
        if not name:
            raise SystemExit(f"{path}:{number}: `{kind}` needs a name")
        if kind == "test":
            if len(name.split()) != 1:
                raise SystemExit(f"{path}:{number}: test needs one filter")
            tests.append((["--workspace", "--lib"], name))
        elif kind == "test-bin":
            parts = name.split()
            if len(parts) != 3 or any(part.startswith("-") for part in parts):
                raise SystemExit(f"{path}:{number}: test-bin needs <package> <binary> <filter>")
            package, binary, filter_name = parts
            tests.append((["--package", package, "--bin", binary], filter_name))
        elif kind == "skip":
            if not reason:
                raise SystemExit(f"{path}:{number}: skip `{name}` needs a reason after ` # `")
            skips.append((name, reason))
        else:
            raise SystemExit(f"{path}:{number}: unknown directive `{kind}`")
    if not tests:
        raise SystemExit(f"{path}: no test filters; the Windows contract would run nothing")
    return tests, skips


def run_contract(tests, skips, profile, dry_run=False):
    for name, reason in skips:
        print(f"SKIPPED {name}: {reason}", flush=True)

    skip_args = [arg for name, _ in skips for arg in ("--skip", name)]
    for target, filter_name in tests:
        command = ["cargo", "test", "--locked", "--profile", profile, *target,
                   filter_name, "--", *skip_args]
        print("+ " + " ".join(command), flush=True)
        if dry_run:
            continue
        result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True)
        print(result.stdout, end="", flush=True)
        if result.returncode != 0:
            print(f"Windows contract failed in filter `{filter_name}`", file=sys.stderr)
            return result.returncode
        # Cargo may run several harnesses, most of which legitimately match nothing. Require
        # actual execution across their summaries, not merely a successful build or listing.
        passed = sum(int(count) for count in re.findall(
            r"^test result: ok\. (\d+) passed;", result.stdout, re.MULTILINE))
        if passed == 0:
            print(f"Windows contract filter `{filter_name}` executed no tests "
                  "(after platform gates, skips and ignored tests)", file=sys.stderr)
            return 1
        print(f"VERIFIED {filter_name}: {passed} tests passed", flush=True)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--profile", default="ci", help="cargo profile (default: ci)")
    parser.add_argument("--print", action="store_true", dest="dry_run",
                        help="print the commands without running them")
    args = parser.parse_args()

    tests, skips = parse(CONTRACT)
    return run_contract(tests, skips, args.profile, args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
