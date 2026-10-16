#!/usr/bin/env python3
"""Run the focused Windows Rust contract listed in scripts/windows-contract.txt.

Each `test` line runs one `cargo test --lib <filter>` invocation; every `skip` line is passed to
libtest as `--skip` and printed with its reason, so excluded coverage is never silent.
`--print` shows the commands without running them.
"""
import argparse
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
            tests.append(name)
        elif kind == "skip":
            if not reason:
                raise SystemExit(f"{path}:{number}: skip `{name}` needs a reason after ` # `")
            skips.append((name, reason))
        else:
            raise SystemExit(f"{path}:{number}: unknown directive `{kind}`")
    if not tests:
        raise SystemExit(f"{path}: no `test` filters; the Windows contract would run nothing")
    return tests, skips


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--profile", default="ci", help="cargo profile (default: ci)")
    parser.add_argument("--print", action="store_true", dest="dry_run",
                        help="print the commands without running them")
    args = parser.parse_args()

    tests, skips = parse(CONTRACT)
    for name, reason in skips:
        print(f"SKIPPED {name}: {reason}", flush=True)

    skip_args = [arg for name, _ in skips for arg in ("--skip", name)]
    for filter_name in tests:
        command = ["cargo", "test", "--locked", "--profile", args.profile, "--lib",
                   filter_name, "--", *skip_args]
        print("+ " + " ".join(command), flush=True)
        if args.dry_run:
            continue
        result = subprocess.run(command, cwd=ROOT)
        if result.returncode != 0:
            print(f"Windows contract failed in filter `{filter_name}`", file=sys.stderr)
            return result.returncode
    return 0


if __name__ == "__main__":
    sys.exit(main())
