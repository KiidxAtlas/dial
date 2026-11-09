#!/usr/bin/env python3
"""Rewrite Formula/dial.rb (and optionally the AUR dial-bin metadata) from a release's SHA256SUMS.

Every Homebrew and AUR target is required; a missing checksum aborts before anything is written.
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
# AUR architecture name -> Rust target triple of the release archive dial-bin installs.
AUR_TARGETS = (
    ("x86_64", "x86_64-unknown-linux-gnu"),
    ("aarch64", "aarch64-unknown-linux-gnu"),
)


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


def aur_digests(version, sums):
    digests = {}
    for arch, target in AUR_TARGETS:
        archive = f"dial-v{version}-{target}.tar.gz"
        if archive not in sums:
            raise SystemExit(f"missing release checksum for {archive}")
        digests[arch] = sums[archive]
    return digests


def substitute_once(pattern, replacement, text, what):
    updated, count = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(f"expected exactly one {what}, found {count}")
    return updated


def update_pkgbuild(text, version, sums):
    """Set pkgver, reset pkgrel to 1 and replace each architecture's checksum."""
    digests = aur_digests(version, sums)
    text = substitute_once(r"^pkgver=.*$", f"pkgver={version}", text, "pkgver line")
    text = substitute_once(r"^pkgrel=.*$", "pkgrel=1", text, "pkgrel line")
    for arch, digest in digests.items():
        text = substitute_once(rf"^sha256sums_{arch}=\(.*\)$", f"sha256sums_{arch}=('{digest}')",
                               text, f"sha256sums_{arch} line")
    return text


def update_srcinfo(text, version, sums):
    """The .SRCINFO that `makepkg --printsrcinfo` would print for the updated PKGBUILD."""
    digests = aur_digests(version, sums)
    text = substitute_once(r"^\tpkgver = .*$", f"\tpkgver = {version}", text, "pkgver entry")
    text = substitute_once(r"^\tpkgrel = .*$", "\tpkgrel = 1", text, "pkgrel entry")
    for arch, target in AUR_TARGETS:
        archive = f"dial-v{version}-{target}.tar.gz"
        text = substitute_once(rf"^\tsource_{arch} = .*$",
                               f"\tsource_{arch} = {REPO_URL}/v{version}/{archive}", text,
                               f"source_{arch} entry")
        text = substitute_once(rf"^\tsha256sums_{arch} = .*$",
                               f"\tsha256sums_{arch} = {digests[arch]}", text,
                               f"sha256sums_{arch} entry")
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--sums", type=Path, required=True)
    parser.add_argument("--formula", type=Path, default=Path("Formula/dial.rb"))
    parser.add_argument("--aur", type=Path, help="AUR package directory holding PKGBUILD and .SRCINFO")
    args = parser.parse_args()
    sums = parse_sums(args.sums.read_text(encoding="utf-8"))
    outputs = {args.formula: update_formula(args.formula.read_text(encoding="utf-8"), args.version, sums)}
    if args.aur:
        pkgbuild, srcinfo = args.aur / "PKGBUILD", args.aur / ".SRCINFO"
        outputs[pkgbuild] = update_pkgbuild(pkgbuild.read_text(encoding="utf-8"), args.version, sums)
        outputs[srcinfo] = update_srcinfo(srcinfo.read_text(encoding="utf-8"), args.version, sums)
    for path, text in outputs.items():
        path.write_text(text, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
