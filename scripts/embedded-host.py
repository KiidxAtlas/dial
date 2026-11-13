#!/usr/bin/env python3
"""Prepare the freshly built dial binary that hosts the embedded shell for Rust test runs.

Library and integration tests run inside libtest processes, so `current_exe()` is a test binary
that does not understand `__embedded-shell`. The embedded shell re-executes the path in
`DIAL_EMBEDDED_SHELL_HOST` instead (crates/dial-tools/src/embedded_shell/mod.rs). This helper:

1. builds `dial` with the same profile and feature flags as the test command (`cargo build -p dial
   --bin dial`), taking the executable path from Cargo's compiler-artifact messages so custom
   `CARGO_TARGET_DIR` values and profile directories are followed without guessing;
2. smoke-checks that executable answers the host protocol with a marker, so a stale or unrelated
   binary cannot stand in for the host;
3. exports only that absolute path to the command. Without the embedded-shell feature there is no
   host: the variable is removed so an inherited value never selects a stale binary.

Usage:
    embedded-host.py [--cargo CMD] [--profile NAME] [--no-default-features] [--features LIST]
                     [--dry-run] -- COMMAND...

Nothing is installed and the user's Dial home is never touched.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

HOST_ENV = "DIAL_EMBEDDED_SHELL_HOST"  # mirrors embedded_shell::HOST_EXE_ENV
HOST_ARG = "__embedded-shell"  # mirrors embedded_shell::HOST_ARG
FEATURE = "embedded-shell"
MARKER = "dial-embedded-host-ok"
SMOKE_TIMEOUT = 120


def embedded_enabled(no_default_features, features):
    """True when this build includes the embedded shell (default features or an explicit name)."""
    if not no_default_features:
        return True
    names = {token.split("/")[-1] for item in features for token in re.split(r"[,\s]+", item) if token}
    return FEATURE in names


def build_command(cargo, profile, no_default_features, features):
    command = [cargo, "build", "--locked", "-p", "dial", "--bin", "dial",
               "--message-format=json-render-diagnostics"]
    if profile:
        command += ["--profile", profile]
    if no_default_features:
        command.append("--no-default-features")
    for item in features:
        command += ["--features", item]
    return command


def executable_from_messages(stdout):
    """Return the one `dial` binary executable named by compiler-artifact messages."""
    found = set()
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        target = message.get("target") or {}
        if target.get("name") == "dial" and "bin" in target.get("kind", []) and message.get("executable"):
            found.add(os.path.abspath(message["executable"]))
    if len(found) != 1:
        raise SystemExit(f"embedded host: expected one dial executable in Cargo's artifacts, found {sorted(found)}")
    return found.pop()


def smoke_check(executable, run=None):
    """Run the host on a one-shot script; it must print the marker and exit 0."""
    run = subprocess.run if run is None else run  # looked up per call so tests can substitute it
    with tempfile.TemporaryDirectory(prefix="dial-host-check-") as directory:
        # Host-owned script name: the host deletes files with the `dial-shell-` prefix after reading.
        script = Path(directory) / f"dial-shell-host-check-{os.getpid()}.sh"
        script.write_text(f"printf '{MARKER}\\n'\n", encoding="utf-8")
        try:
            result = run([executable, HOST_ARG, str(script)], capture_output=True, text=True,
                         timeout=SMOKE_TIMEOUT)
        except (OSError, subprocess.SubprocessError) as error:
            raise SystemExit(f"embedded host smoke check could not run {executable}: {error}")
    if result.returncode != 0 or MARKER not in result.stdout:
        raise SystemExit(f"embedded host smoke check failed for {executable} "
                         f"(exit {result.returncode}): {result.stdout}{result.stderr}")


def prepare(cargo, profile, no_default_features=False, features=(), dry_run=False, run=None, out=None):
    """Build and verify the host; return its path, or None when the build has no embedded shell."""
    # Resolved per call (not as default arguments) so patched subprocess.run and redirected stdout apply.
    run = subprocess.run if run is None else run
    out = sys.stdout if out is None else out
    if not embedded_enabled(no_default_features, list(features)):
        print(f"embedded host: embedded-shell is not in this build; {HOST_ENV} is not set", file=out, flush=True)
        return None
    command = build_command(cargo, profile, no_default_features, list(features))
    print("+ " + " ".join(command), file=out, flush=True)
    if dry_run:
        print("+ smoke-check the built host with the " + MARKER + " marker", file=out, flush=True)
        return None
    result = run(command, stdout=subprocess.PIPE, text=True)
    if result.returncode != 0:
        raise SystemExit(f"embedded host build failed with exit {result.returncode}")
    executable = executable_from_messages(result.stdout)
    smoke_check(executable, run=run)
    print(f"embedded host: {executable}", file=out, flush=True)
    return executable


def environment(host, base=None):
    """The environment for a test command: the verified host path, or no host variable at all."""
    env = dict(os.environ if base is None else base)
    if host:
        env[HOST_ENV] = host
    else:
        env.pop(HOST_ENV, None)
    return env


def main(argv):
    if "--" not in argv:
        raise SystemExit("usage: embedded-host.py [options] -- COMMAND...")
    split = argv.index("--")
    options, command = argv[:split], argv[split + 1:]
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--cargo", default="cargo", help="cargo executable (Makefile passes CARGO)")
    parser.add_argument("--profile", help="cargo profile for the host build (omit for cargo's default)")
    parser.add_argument("--no-default-features", action="store_true")
    parser.add_argument("--features", action="append", default=[])
    parser.add_argument("--dry-run", action="store_true", help="print the host build, do not run it")
    args = parser.parse_args(options)
    if not command:
        raise SystemExit("embedded host: no command after `--`")
    host = prepare(args.cargo, args.profile, args.no_default_features, args.features, args.dry_run)
    if args.dry_run:
        return 0
    return subprocess.run(command, env=environment(host)).returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
