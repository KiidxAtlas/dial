#!/usr/bin/env python3
"""Fail early with actionable prerequisites for the selected test tier.

Checks never install dependencies. Native Rust enforcement probes still run in the suite.
"""
import argparse
import importlib
import os
import subprocess
import sys

# Match the native backend: finding a different bwrap on PATH is not sufficient.
BWRAP = "/usr/bin/bwrap"
PROBE = [BWRAP, "--die-with-parent", "--new-session", "--unshare-user",
         "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-net",
         "--cap-drop", "ALL", "--ro-bind", "/", "/", "--proc", "/proc",
         "--dev", "/dev", "--", "/bin/true"]


def linux_problems():
    problems = []
    unprivileged = os.getuid() != 0 and os.geteuid() != 0
    if not unprivileged:
        problems.append("Run tests as a normal unprivileged user, not root (including sudo).")
    installed = os.path.isfile(BWRAP) and os.access(BWRAP, os.X_OK)
    if not installed:
        problems.append(f"Install bubblewrap with an executable at {BWRAP} (e.g. apt install bubblewrap).")
    if unprivileged and installed:
        guidance = ("Allow unprivileged user namespaces and bubblewrap in the host/container security "
                    "policy (including seccomp/AppArmor); use a suitable Linux/WSL2 host if unavailable.")
        try:
            result = subprocess.run(PROBE, capture_output=True, text=True, timeout=10,
                                    env={"PATH": "/usr/bin:/bin", "LANG": "C"}, check=False)
        except (OSError, subprocess.TimeoutExpired) as error:
            problems.append(f"Bubblewrap namespace probe could not complete: {error}. {guidance}")
        else:
            if result.returncode != 0:
                detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic output"
                problems.append(f"Bubblewrap namespace probe failed (exit {result.returncode}): "
                                f"{detail}. {guidance}")
    return problems


def python_prerequisites(suite):
    modules = ("pytest", "fastapi") if suite == "python" else ("cryptography.hazmat.primitives.asymmetric.ed25519",)
    for module in modules:
        try:
            importlib.import_module(module)
        except (ImportError, OSError) as error:
            print(f"{suite} tests cannot start: {sys.executable} cannot import {module}: {error}", file=sys.stderr)
            print("Use the pinned CI environment (no system Python changes):", file=sys.stderr)
            print(f"  uv run --no-project --python 3.12 --with-requirements scripts/requirements-ci.lock make test-{suite}", file=sys.stderr)
            print("Or select an existing prepared interpreter with make PYTHON=/path/to/python.", file=sys.stderr)
            print("No tests ran; this is a prerequisite failure, not a passing or skipped suite.", file=sys.stderr)
            return 1
    print(f"{suite} test prerequisites available in {sys.executable}.")
    return 0


def main(suite="rust"):
    if suite != "rust":
        return python_prerequisites(suite)
    if sys.platform != "linux":
        print("Linux sandbox prerequisite gate is not applicable on this platform; native Rust checks remain required.")
        return 0
    problems = linux_problems()
    if problems:
        print("Rust tests cannot start: Linux sandbox prerequisites are unavailable:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print("No Rust tests ran; this is a prerequisite failure, not a sandbox-test skip.", file=sys.stderr)
        return 1
    print("Linux sandbox prerequisites available: unprivileged user, bubblewrap, working namespace probe.")
    return 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("suite", nargs="?", choices=("rust", "python", "release-policy"), default="rust")
    sys.exit(main(parser.parse_args().suite))
