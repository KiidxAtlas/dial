#!/usr/bin/env python3
"""Rewrite Formula/dial.rb's version and on_macos/on_linux stanzas from a release's SHA256SUMS.

Every Homebrew target is required; a missing checksum aborts without touching the formula.
"""
import argparse
import re
import sys
from pathlib import Path

REPO_URL = "https://github.com/KiidxAtlas/dial/releases/download"
# (os block, architecture block, Rust target triple) in Homebrew stanza order.
HOMEBREW_TARGETS = (
    ("on_macos", "on_arm", "aarch64-apple-darwin"),
    ("on_macos", "on_intel", "x86_64-apple-darwin"),
    ("on_linux", "on_arm", "aarch64-unknown-linux-gnu"),
    ("on_linux", "on_intel", "x86_64-unknown-linux-gnu"),
)
VERSION_LINE = re.compile(r'^  version ".*"$', re.MULTILINE)
PLATFORM_BLOCKS = re.compile(r"^  on_macos do\n.*?^  on_linux do\n.*?^  end\n", re.MULTILINE | re.DOTALL)


def parse_sums(text):
    sums = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        digest, name = line.split(maxsplit=1)
        sums[name.lstrip("*")] = digest
    return sums


def render_platform_blocks(version, sums):
    lines = []
    for os_block in ("on_macos", "on_linux"):
        lines.append(f"  {os_block} do")
        for block_os, arch_block, target in HOMEBREW_TARGETS:
            if block_os != os_block:
                continue
            archive = f"dial-v{version}-{target}.tar.gz"
            if archive not in sums:
                raise SystemExit(f"missing release checksum for {archive}")
            lines += [
                f"    {arch_block} do",
                f'      url "{REPO_URL}/v#{{version}}/dial-v#{{version}}-{target}.tar.gz"',
                f'      sha256 "{sums[archive]}"',
                "    end",
            ]
        lines.append("  end")
    return "\n".join(lines) + "\n"


def update_formula(formula_text, version, sums):
    if len(VERSION_LINE.findall(formula_text)) != 1:
        raise SystemExit("Formula must declare exactly one `  version` line")
    if len(PLATFORM_BLOCKS.findall(formula_text)) != 1:
        raise SystemExit("Formula must contain exactly one on_macos/on_linux block pair")
    updated = VERSION_LINE.sub(f'  version "{version}"', formula_text, count=1)
    return PLATFORM_BLOCKS.sub(lambda _: render_platform_blocks(version, sums), updated, count=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--sums", type=Path, required=True)
    parser.add_argument("--formula", type=Path, default=Path("Formula/dial.rb"))
    args = parser.parse_args()
    sums = parse_sums(args.sums.read_text(encoding="utf-8"))
    formula = args.formula.read_text(encoding="utf-8")
    args.formula.write_text(update_formula(formula, args.version, sums), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
