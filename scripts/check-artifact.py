#!/usr/bin/env python3
"""Offline contracts against an installed/package executable outside the checkout."""
import argparse
import hashlib
import json
import os
import subprocess
import tempfile
from pathlib import Path


ELF_MACHINES = {"x86_64": 0x3E, "aarch64": 0xB7}
MACH_O_CPU_TYPES = {"x86_64": 0x01000007, "aarch64": 0x0100000C}
PE_MACHINES = {"x86_64": 0x8664, "aarch64": 0xAA64}
PT_INTERP = 3


def elf_interpreter(data):
    """Returns the PT_INTERP loader path of a 64-bit little-endian ELF, or None when there is none."""
    program_table = int.from_bytes(data[32:40], "little")
    entry_size = int.from_bytes(data[54:56], "little")
    entry_count = int.from_bytes(data[56:58], "little")
    for index in range(entry_count):
        entry = program_table + index * entry_size
        if int.from_bytes(data[entry:entry + 4], "little") == PT_INTERP:
            offset = int.from_bytes(data[entry + 8:entry + 16], "little")
            size = int.from_bytes(data[entry + 32:entry + 40], "little")
            return data[offset:offset + size].rstrip(b"\0").decode("utf-8", "replace")
    return None


def check_target_format(executable, target):
    """Fails unless the binary header matches the release target's OS, CPU, and C library."""
    arch = target.partition("-")[0]
    data = executable.read_bytes()
    if "apple-darwin" in target:
        cpu = int.from_bytes(data[4:8], "little") if data[:4] == b"\xcf\xfa\xed\xfe" else None
        if arch not in MACH_O_CPU_TYPES or cpu != MACH_O_CPU_TYPES[arch]:
            raise RuntimeError(f"{executable.name} is not a 64-bit Mach-O {arch} binary")
    elif "windows" in target:
        pe = int.from_bytes(data[0x3C:0x40], "little") if data[:2] == b"MZ" else None
        if pe is None or data[pe:pe + 4] != b"PE\0\0" or arch not in PE_MACHINES \
                or int.from_bytes(data[pe + 4:pe + 6], "little") != PE_MACHINES[arch]:
            raise RuntimeError(f"{executable.name} is not a PE {arch} binary")
    elif "linux" in target:
        if data[:5] != b"\x7fELF\x02\x01" or arch not in ELF_MACHINES \
                or int.from_bytes(data[18:20], "little") != ELF_MACHINES[arch]:
            raise RuntimeError(f"{executable.name} is not a 64-bit little-endian ELF {arch} binary")
        interpreter = elf_interpreter(data)
        if target.endswith("-musl"):
            if interpreter is not None and "ld-musl" not in interpreter:
                raise RuntimeError(f"musl target links the glibc loader {interpreter!r}")
        elif interpreter is None or "ld-linux" not in interpreter:
            raise RuntimeError(f"glibc target {target} does not link ld-linux (got {interpreter!r})")
    else:
        raise RuntimeError(f"unknown release target {target!r}")



def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dial", type=Path, required=True)
    parser.add_argument("--target", help="release target triple; verifies the binary format for it")
    parser.add_argument("--format-only", action="store_true",
                        help="verify only the --target binary format without executing the artifact")
    args = parser.parse_args()
    executable = args.dial.resolve(strict=True)
    checks = []
    if args.target:
        check_target_format(executable, args.target)
        checks.append("target-format")
    if args.format_only:
        if not args.target:
            parser.error("--format-only requires --target")
        print(json.dumps({"schema_version": 1, "executable": str(executable), "checks": checks}))
        return
    with tempfile.TemporaryDirectory(prefix="dial-artifact-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "dial-home"
        env = os.environ.copy()
        env["DIAL_HOME"] = str(home)
        env.pop("DIAL_EMBEDDED_SHELL_HOST", None)

        def run(*arguments, cwd=workspace):
            result = subprocess.run([str(executable), *map(str, arguments)], cwd=cwd,
                                    env=env, capture_output=True, text=True, timeout=30)
            if result.returncode:
                raise RuntimeError(f"artifact command {arguments!r} failed: {result.stderr[-2048:]}")
            return result.stdout

        assert run("--version").startswith("dial ")
        assert "annotate" in run("--help")
        reference = json.loads(run("config", "--json", "reference"))
        assert reference["schema_version"] == 1
        settings = {entry["path"]: entry for entry in reference["settings"]}
        assert settings["harness.max_steps"]["default"] == 0
        assert "ui.pinned_workers" in settings and "memory.backend" in settings
        for shell in ("bash", "zsh", "fish", "powershell", "elvish"):
            assert "dial" in run("completions", shell)

        # Exercises the actual re-executed shell host and bundled utility registrations.
        script = root / "dial-shell-artifact.sh"
        script.write_text("printf 'alpha\\nbeta\\n' | head -n 1\n", encoding="utf-8")
        assert run("__embedded-shell", script).strip() == "alpha"
        assert not script.exists()

        # Re-executed hosts retain only completed, validated cwd/export state.
        state = root / "shell-state.json"
        seed = {"schema_version": 1, "completed": False, "cwd": str(workspace), "exported": {}}
        state.write_text(json.dumps(seed), encoding="utf-8")
        state.chmod(0o600)
        script.write_text("mkdir nested; cd nested; export HARNESS_FIXTURE_VALUE=retained\n", encoding="utf-8")
        run("__embedded-shell", script, state)
        saved = json.loads(state.read_text())
        assert saved["completed"] and Path(saved["cwd"]).resolve() == (workspace / "nested").resolve()
        assert saved["exported"]["HARNESS_FIXTURE_VALUE"] == "retained"
        script.write_text('printf "%s\\n" "$HARNESS_FIXTURE_VALUE"; pwd\n', encoding="utf-8")
        assert run("__embedded-shell", script, state, cwd=Path(saved["cwd"])).splitlines() == ["retained", saved["cwd"]]
        assert not script.exists()

        # Discover the actual project state path through the CLI, then annotate immutable output.
        # Config listing does not need model credentials and preserves the zero-step convention.
        run("config", "set", "harness.max_steps", "0", "--project")
        states = list((home / "projects").iterdir())
        assert len(states) == 1
        artifacts = states[0] / "artifacts"
        artifacts.mkdir(mode=0o700)
        source = b"first\nsecond\n"
        identity = hashlib.sha256(source).hexdigest()
        artifact = artifacts / identity
        artifact.write_bytes(source)
        artifact.chmod(0o600)
        uri = f"artifact://{identity}"
        note = json.loads(run("annotate", uri, "--note", "Check the second row", "--start", "2"))
        assert note["sha256"] == identity and note["start"] == note["end"] == 2
        notes = json.loads(run("annotate", uri, "--list"))
        assert len(notes["annotations"]) == 1 and notes["annotations"][0]["origin"] == "human_cli"
        assert artifact.read_bytes() == source
        print(json.dumps({"schema_version": 1, "executable": str(executable),
                          "executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
                          "checks": [*checks, "entry", "metadata", "completions", "shell-host", "shell-state", "annotation"]}))


if __name__ == "__main__":
    main()
