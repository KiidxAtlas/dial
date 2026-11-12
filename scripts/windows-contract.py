#!/usr/bin/env python3
"""Run a focused native Rust contract: scripts/windows-contract.txt by default, or --contract.

Each `test-lib` selects a package library; `test-bin` selects a package binary; `test-integration`
selects a package integration-test target. Every `skip` line is passed to libtest as `--skip` and
printed with its reason. Successful Cargo exits with no executed tests are rejected, including
ignored-only selections. `--print` shows the commands without running them (it does not verify
selection).
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
        elif kind == "test-lib":
            parts = name.split()
            if len(parts) != 2 or any(part.startswith("-") for part in parts):
                raise SystemExit(f"{path}:{number}: test-lib needs <package> <filter>")
            package, filter_name = parts
            tests.append((["--package", package, "--lib"], filter_name))
        elif kind == "test-bin":
            parts = name.split()
            if len(parts) != 3 or any(part.startswith("-") for part in parts):
                raise SystemExit(f"{path}:{number}: test-bin needs <package> <binary> <filter>")
            package, binary, filter_name = parts
            tests.append((["--package", package, "--bin", binary], filter_name))
        elif kind == "test-integration":
            parts = name.split()
            if len(parts) != 3 or any(part.startswith("-") for part in parts):
                raise SystemExit(f"{path}:{number}: test-integration needs <package> <target> <filter>")
            package, target, filter_name = parts
            tests.append((["--package", package, "--test", target], filter_name))
        elif kind == "skip":
            if not reason:
                raise SystemExit(f"{path}:{number}: skip `{name}` needs a reason after ` # `")
            skips.append((name, reason))
        else:
            raise SystemExit(f"{path}:{number}: unknown directive `{kind}`")
    if not tests:
        raise SystemExit(f"{path}: no test filters; the contract would run nothing")
    return tests, skips


def run_contract(tests, skips, profile, dry_run=False, label="Windows"):
    for name, reason in skips:
        print(f"SKIPPED {name}: {reason}", flush=True)

    skip_args = [arg for name, _ in skips for arg in ("--skip", name)]
    # Every filter runs even after one fails, so a single CI run reports all Windows failures;
    # the first failure's exit status is preserved.
    status = 0
    failed = []
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
            print(f"{label} contract failed in filter `{filter_name}`", file=sys.stderr)
            status = status or result.returncode
            failed.append(filter_name)
            continue
        # Cargo may run several harnesses, most of which legitimately match nothing. Require
        # actual execution across their summaries, not merely a successful build or listing.
        passed = sum(int(count) for count in re.findall(
            r"^test result: ok\. (\d+) passed;", result.stdout, re.MULTILINE))
        if passed == 0:
            print(f"{label} contract filter `{filter_name}` executed no tests "
                  "(after platform gates, skips and ignored tests)", file=sys.stderr)
            status = status or 1
            failed.append(filter_name)
            continue
        print(f"VERIFIED {filter_name}: {passed} tests passed", flush=True)
    if failed:
        print(f"{label} contract failures: " + ", ".join(f"`{name}`" for name in failed),
              file=sys.stderr)
    return status


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--profile", default="ci", help="cargo profile (default: ci)")
    parser.add_argument("--contract", type=Path, default=CONTRACT,
                        help="contract file (default: scripts/windows-contract.txt)")
    parser.add_argument("--label", default="Windows", help="platform name used in messages")
    parser.add_argument("--print", action="store_true", dest="dry_run",
                        help="print the commands without running them")
    args = parser.parse_args()

    tests, skips = parse(args.contract)
    return run_contract(tests, skips, args.profile, args.dry_run, args.label)


if __name__ == "__main__":
    sys.exit(main())
