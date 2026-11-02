#!/usr/bin/env python3
"""Fail early with one diagnosis when Linux Rust sandbox tests cannot run.

This checks prerequisites, not Dial's full enforcement policy. The Rust sandbox tests
still run their real enforcement probes. Other platforms retain their native checks.
"""
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


def main():
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
    sys.exit(main())
