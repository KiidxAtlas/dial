#!/usr/bin/env python3
"""Measure a full-featured release build without changing the shipping profile."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time


def measure(command, repetitions=1):
    samples = []
    for _ in range(repetitions):
        started = time.perf_counter()
        subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
        samples.append(time.perf_counter() - started)
    return samples


def benchmark(target, lto, target_dir):
    env = dict(os.environ, CARGO_PROFILE_RELEASE_LTO='thin' if lto == 'thin' else 'false',
               CARGO_TARGET_DIR=str(target_dir.resolve()))
    started = time.perf_counter()
    subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'dial', '--bin', 'dial',
                    '--target', target, '--timings'], env=env, check=True)
    build_seconds = time.perf_counter() - started
    binary = target_dir.resolve() / target / 'release' / ('dial.exe' if 'windows' in target else 'dial')
    # This benchmark must run on the target host, not under an unrecorded cross-runtime.
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    measure([str(binary), '--help'])
    samples = measure([str(binary), '--help'], repetitions=7)
    return dict(target=target, lto=lto, build_seconds=build_seconds,
                binary_bytes=binary.stat().st_size,
                sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), version=version,
                help_seconds=samples, help_median_seconds=statistics.median(samples),
                rustc=subprocess.check_output(['rustc', '--version'], text=True).strip(),
                revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                dirty=bool(subprocess.check_output(['git', 'status', '--porcelain'], text=True).strip()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', required=True, help='Native Rust target triple')
    parser.add_argument('--lto', choices=('thin', 'off'), required=True)
    parser.add_argument('--target-dir', type=Path, help='Use an empty directory for a cold build')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = benchmark(args.target, args.lto, args.target_dir or Path('target/release-benchmark') / args.lto)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    assert json.loads(args.output.read_text()) == result
    assert result['build_seconds'] >= 0 and result['binary_bytes'] > 0
    assert len(result['help_seconds']) == 7 and result['help_median_seconds'] >= 0
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
