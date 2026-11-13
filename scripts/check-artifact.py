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
PE32_PLUS_MAGIC = 0x20B
NODE_MANIFEST = Path(__file__).resolve().parent.parent / "runtimes" / "node" / "manifest.json"
SMOKE_REQUIRED_CHECKS = ("bundled-node", "node-version", "computer-worker")
# Poison values: a probe that consults the host's Node through these must fail, not borrow it.
POISON_NODE_OPTIONS = "--dial-poison-node-options-must-not-be-read"
POISON_NODE_PATH = "/dial-poison-node-path-must-not-be-read"
PE_IMPORT_DIRECTORY = 1
PE_DELAY_IMPORT_DIRECTORY = 13
# Visual C++ runtime redistributable DLLs (and the UCRT they sit on). OS API sets stay allowed.
VC_RUNTIME_DLL_PREFIXES = ("vcruntime", "msvcp", "concrt", "ucrtbase", "api-ms-win-crt-")
MAX_PE_IMPORT_TABLE_ENTRIES = 512
MAX_PE_DLL_NAME_BYTES = 256
PE_MAX_DIRECTORIES = 16


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


def pe_bytes(data, offset, size):
    """Returns exactly size bytes at offset, failing closed when the range leaves the file."""
    if offset < 0 or size < 0 or offset + size > len(data):
        raise RuntimeError(f"PE table at file offset {offset:#x} lies outside the file")
    return data[offset:offset + size]


def pe_section_window(sections, rva):
    """Returns (file offset, bytes left in that section's raw data) for an RVA inside a section."""
    for virtual_address, raw_size, raw_pointer in sections:
        if virtual_address <= rva < virtual_address + raw_size:
            return raw_pointer + rva - virtual_address, virtual_address + raw_size - rva
    raise RuntimeError(f"PE RVA {rva:#x} lies outside every section's raw data")


def pe_section_entry(data, sections, rva, size):
    """Reads one fixed-size table entry that must lie wholly inside a single section's raw data."""
    offset, left = pe_section_window(sections, rva)
    if size > left:
        raise RuntimeError(f"PE table entry at RVA {rva:#x} crosses its section's raw data")
    return pe_bytes(data, offset, size)


def pe_dll_name(data, sections, rva):
    offset, left = pe_section_window(sections, rva)
    window = pe_bytes(data, offset, min(MAX_PE_DLL_NAME_BYTES, left))
    end = window.find(b"\0")
    if end <= 0:
        raise RuntimeError(f"PE import DLL name at RVA {rva:#x} is unterminated or empty within its section")
    try:
        return window[:end].decode("ascii")
    except UnicodeDecodeError as error:
        raise RuntimeError(f"PE import DLL name at RVA {rva:#x} is not ASCII") from error


def pe_imported_dlls(data, pe):
    """Returns (normal, delay) DLL names imported by a PE32+ image whose PE signature is at pe."""
    coff = pe + 4
    section_count = int.from_bytes(pe_bytes(data, coff + 2, 2), "little")
    optional_size = int.from_bytes(pe_bytes(data, coff + 16, 2), "little")
    optional = pe + 24
    directory_count = int.from_bytes(pe_bytes(data, optional + 108, 4), "little")
    if directory_count > PE_MAX_DIRECTORIES:
        raise RuntimeError(f"PE declares {directory_count} data directories; the format allows {PE_MAX_DIRECTORIES}")
    if optional_size < 112 + 8 * directory_count:
        raise RuntimeError("PE optional header is too small to hold its declared data directories")
    section_table = optional + optional_size
    sections = []
    for index in range(section_count):
        header = pe_bytes(data, section_table + 40 * index, 40)
        virtual_address = int.from_bytes(header[12:16], "little")
        raw_size = int.from_bytes(header[16:20], "little")
        raw_pointer = int.from_bytes(header[20:24], "little")
        sections.append((virtual_address, raw_size, raw_pointer))

    def directory(index):
        if index >= directory_count:
            return 0, 0
        entry = pe_bytes(data, optional + 112 + 8 * index, 8)
        rva, size = int.from_bytes(entry[:4], "little"), int.from_bytes(entry[4:], "little")
        if bool(rva) != bool(size):
            raise RuntimeError(f"PE data directory {index} is inconsistent (RVA {rva:#x}, size {size})")
        return rva, size

    def walk(rva, size, entry_size, name_at, check_entry):
        """Walks descriptors until an all-zero terminator, which must fit inside the declared size."""
        names = []
        for index in range(MAX_PE_IMPORT_TABLE_ENTRIES):
            if (index + 1) * entry_size > size:
                raise RuntimeError("PE import table does not terminate within its declared size")
            entry = pe_section_entry(data, sections, rva + index * entry_size, entry_size)
            if not any(entry):
                return names
            check_entry(entry)
            names.append(pe_dll_name(data, sections, int.from_bytes(entry[name_at:name_at + 4], "little")))
        raise RuntimeError(f"PE import table does not terminate within {MAX_PE_IMPORT_TABLE_ENTRIES} entries")

    def check_delay(entry):
        if not int.from_bytes(entry[:4], "little") & 1:
            raise RuntimeError("PE delay-load descriptor uses absolute addresses (unsupported)")

    normal_rva, normal_size = directory(PE_IMPORT_DIRECTORY)
    delay_rva, delay_size = directory(PE_DELAY_IMPORT_DIRECTORY)
    normal = walk(normal_rva, normal_size, 20, 12, lambda entry: None) if normal_rva else []
    delayed = walk(delay_rva, delay_size, 32, 4, check_delay) if delay_rva else []
    return normal, delayed


def check_pe_runtime_imports(executable, data, pe):
    normal, delayed = pe_imported_dlls(data, pe)
    for dll in (*normal, *delayed):
        if dll.lower().startswith(VC_RUNTIME_DLL_PREFIXES):
            raise RuntimeError(f"{executable.name} imports the Visual C++ runtime DLL {dll}; "
                               "link the C runtime statically")


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
                or int.from_bytes(data[pe + 4:pe + 6], "little") != PE_MACHINES[arch] \
                or int.from_bytes(data[pe + 24:pe + 26], "little") != PE32_PLUS_MAGIC:
            raise RuntimeError(f"{executable.name} is not a PE32+ {arch} binary")
        check_pe_runtime_imports(executable, data, pe)
    elif "linux" in target:
        if data[:6] != b"\x7fELF\x02\x01" or arch not in ELF_MACHINES \
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



def expected_node_version():
    """Returns the exact Node version pinned in runtimes/node/manifest.json, v-prefixed as the probe reports it."""
    version = json.loads(NODE_MANIFEST.read_text(encoding="utf-8")).get("version")
    if not isinstance(version, str) or not version:
        raise RuntimeError(f"{NODE_MANIFEST} needs a non-empty string 'version'")
    return f"v{version}"


def check_installed_smoke_report(text, expected_version):
    """Returns the probe report only when it proves the bundled payload and the computer worker; fails closed otherwise."""
    try:
        report = json.loads(text)
    except ValueError as error:
        raise RuntimeError(f"__installed-smoke printed no JSON report: {error}") from error
    if not isinstance(report, dict) or report.get("schema_version") != 1:
        raise RuntimeError(f"__installed-smoke report has an unexpected schema: {report!r}")
    if report.get("bundled") is not True:
        raise RuntimeError(f"packaged Node payload is not bundled (bundled={report.get('bundled')!r})")
    if report.get("node_version") != expected_version:
        raise RuntimeError(f"bundled Node is {report.get('node_version')!r}; the manifest pins {expected_version!r}")
    checks = report.get("checks")
    missing = [name for name in SMOKE_REQUIRED_CHECKS if not isinstance(checks, list) or name not in checks]
    if missing:
        raise RuntimeError(f"__installed-smoke did not prove {', '.join(missing)}")
    return report


def installed_smoke(executable, root, expected_version):
    """Runs the packaged probe twice under one DIAL_HOME: a fresh run, then a run reusing its cache.

    Both runs get an empty PATH and poisoned Node variables, so only the bundled payload can satisfy them.
    """
    home = root / "bundled-home"
    # The probe's workspace is a sibling of DIAL_HOME, never an ancestor: the runtime home must not sit inside the cwd.
    workspace = root / "bundled-workspace"
    workspace.mkdir()
    reports = []
    for _ in range(2):
        env = os.environ.copy()
        env.update(PATH="", NODE_OPTIONS=POISON_NODE_OPTIONS, NODE_PATH=POISON_NODE_PATH, DIAL_HOME=str(home))
        env.pop("DIAL_EMBEDDED_SHELL_HOST", None)
        result = subprocess.run([str(executable), "__installed-smoke", "--require-bundled-node"],
                                cwd=workspace, env=env, capture_output=True, text=True, timeout=180)
        if result.returncode:
            raise RuntimeError(f"__installed-smoke failed (exit {result.returncode}): {result.stderr[-2048:]}")
        reports.append(check_installed_smoke_report(result.stdout, expected_version))
    if reports[0] != reports[1]:
        raise RuntimeError("__installed-smoke changed between the fresh run and the cached run")
    return reports[-1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dial", type=Path, required=True)
    parser.add_argument("--target", help="release target triple; verifies the binary format for it")
    parser.add_argument("--format-only", action="store_true",
                        help="verify only the --target binary format without executing the artifact")
    parser.add_argument("--require-bundled-node", action="store_true",
                        help="also run __installed-smoke; fail unless the packaged Node payload and computer worker are proven")
    args = parser.parse_args()
    if args.format_only and args.require_bundled_node:
        parser.error("--require-bundled-node cannot be combined with --format-only")
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
        # Shell scripts use LF on every OS, as Dial writes them; Windows text mode would add CR.
        script = root / "dial-shell-artifact.sh"
        script.write_text("printf 'alpha\\nbeta\\n' | head -n 1\n", encoding="utf-8", newline="\n")
        assert run("__embedded-shell", script).strip() == "alpha"
        assert not script.exists()

        # Re-executed hosts retain only completed, validated cwd/export state.
        state = root / "shell-state.json"
        seed = {"schema_version": 1, "completed": False, "cwd": str(workspace), "exported": {}}
        state.write_text(json.dumps(seed), encoding="utf-8")
        state.chmod(0o600)
        script.write_text("mkdir nested; cd nested; export HARNESS_FIXTURE_VALUE=retained\n", encoding="utf-8", newline="\n")
        run("__embedded-shell", script, state)
        saved = json.loads(state.read_text())
        assert saved["completed"] and Path(saved["cwd"]).resolve() == (workspace / "nested").resolve()
        assert saved["exported"]["HARNESS_FIXTURE_VALUE"] == "retained"
        script.write_text('printf "%s\\n" "$HARNESS_FIXTURE_VALUE"; pwd\n', encoding="utf-8", newline="\n")
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
        report = None
        if args.require_bundled_node:
            report = installed_smoke(executable, root, expected_node_version())
            checks.append("installed-smoke")
        print(json.dumps({"schema_version": 1, "executable": str(executable),
                          "executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
                          "checks": [*checks, "entry", "metadata", "completions", "shell-host", "shell-state", "annotation"],
                          **({"installed_smoke": report} if report else {})}))


if __name__ == "__main__":
    main()
