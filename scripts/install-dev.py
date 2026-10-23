#!/usr/bin/env python3
"""Development install used by `make update` (not the signed `dial update` path).

Guarantees, in order:
- `check` validates retention input before anything is built.
- `install` takes a rollback copy of every existing managed binary first. Any backup failure stops
  the run before a destination is touched. Backup names are collision-safe: a second copy in the
  same second gets a `-NN` suffix instead of overwriting the older rollback point.
- Each destination is staged beside itself, then activated with an atomic rename. If activation or
  the byte-for-byte post-check fails, every location activated so far is restored from its backup.
- `prune` removes old backups only after the caller has verified the active install.
"""
import argparse
import filecmp
import os
import shutil
import sys
import time
from pathlib import Path


class InstallError(Exception):
    pass


def positive_keep(value):
    if not value.isdigit() or int(value) < 1:
        raise InstallError(f"KEEP_BACKUPS must be a positive integer, got {value!r}")
    return int(value)


def managed_destinations(bin_target, install_dir, cargo_bin):
    """Checkout copy, installed copy, and the Cargo copy when it exists outside install_dir."""
    destinations = [Path(bin_target), Path(install_dir) / "dial"]
    cargo = Path(cargo_bin)
    if cargo.is_file() and cargo.parent != Path(install_dir):
        destinations.append(cargo)
    return list(dict.fromkeys(destinations))


def unique_backup(dest, stamp):
    """A backup path next to `dest` that does not exist yet: `dial.bak-STAMP`, then `-02`, `-03`…"""
    base = dest.with_name(f"{dest.name}.bak-{stamp}")
    candidate, number = base, 1
    while candidate.exists() or candidate.is_symlink():
        number += 1
        candidate = base.with_name(f"{base.name}-{number:02d}")
    return candidate


def restore(paths, backups):
    """Put back every path's rollback copy (or remove a fresh install). Returns failure messages."""
    failures = []
    for dest in paths:
        try:
            backup = backups.get(dest)
            if backup is not None:
                os.replace(backup, dest)
            else:
                dest.unlink(missing_ok=True)
        except OSError as error:
            failures.append(f"{dest}: {error}")
    return failures


def install(build, destinations, stamp, replace=os.replace):
    """Back up, stage, activate and verify. Returns {destination: backup path} on success."""
    build = Path(build)
    if not build.is_file():
        raise InstallError(f"build output is missing: {build}")

    backups = {}
    for dest in destinations:
        if not dest.exists():
            continue
        backup = unique_backup(dest, stamp)
        try:
            shutil.copy2(dest, backup)
            if not filecmp.cmp(dest, backup, shallow=False):
                raise OSError("rollback copy does not match the existing binary")
        except OSError as error:
            backup.unlink(missing_ok=True)
            raise InstallError(f"backup of {dest} to {backup} failed: {error}") from error
        backups[dest] = backup
        print(f"Preserved previous binary: {backup}")

    stages = {}
    current = None
    try:
        for dest in destinations:
            current = dest
            dest.parent.mkdir(parents=True, exist_ok=True)
            stage = dest.with_name(f".{dest.name}.new-{os.getpid()}")
            shutil.copyfile(build, stage)
            os.chmod(stage, 0o755)
            stages[dest] = stage
    except OSError as error:
        for stage in stages.values():
            stage.unlink(missing_ok=True)
        raise InstallError(f"staging {current} failed; no installed binary was changed: {error}") from error

    activated = []
    current = None
    try:
        for dest in destinations:
            current = dest
            replace(stages[dest], dest)
            activated.append(dest)
    except OSError as error:
        for stage in stages.values():
            stage.unlink(missing_ok=True)
        failures = restore(activated, backups)
        detail = "; ".join(failures) or "previous binaries restored"
        raise InstallError(f"activating {current} failed ({error}); {detail}") from error

    try:
        for dest in destinations:
            current = dest
            if not filecmp.cmp(build, dest, shallow=False):
                raise OSError("does not match the build after install")
    except OSError as error:
        failures = restore(activated, backups)
        detail = "; ".join(failures) or "previous binaries restored"
        raise InstallError(f"verifying {current} failed ({error}); {detail}") from error
    return backups


def prune(destinations, keep):
    for dest in destinations:
        if not dest.parent.is_dir():
            continue
        prefix = f"{dest.name}.bak-"
        backups = sorted(
            (p for p in dest.parent.iterdir() if p.name.startswith(prefix) and p.is_file()),
            key=str,
            reverse=True,
        )
        for old in backups[keep:]:
            old.unlink()
            print(f"Pruned old backup: {old}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    check = commands.add_parser("check", help="validate KEEP_BACKUPS before building")
    check.add_argument("--keep", required=True)

    for name in ("install", "prune"):
        sub = commands.add_parser(name)
        sub.add_argument("--keep", required=True)
        sub.add_argument("--bin-target", required=True)
        sub.add_argument("--install-dir", required=True)
        sub.add_argument("--cargo-bin", required=True)
        if name == "install":
            sub.add_argument("--build", required=True)
            sub.add_argument("--stamp", default=time.strftime("%Y%m%d-%H%M%S"))

    args = parser.parse_args(argv)
    try:
        keep = positive_keep(args.keep)
        if args.command == "check":
            return 0
        destinations = managed_destinations(args.bin_target, args.install_dir, args.cargo_bin)
        if args.command == "install":
            install(args.build, destinations, args.stamp)
        else:
            prune(destinations, keep)
    except InstallError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
