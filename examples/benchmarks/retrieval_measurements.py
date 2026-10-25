#!/usr/bin/env python3
"""Run real Dial offline retrieval experiments; standard library only, no model/network.

Usage: python3 examples/benchmarks/retrieval_measurements.py --output /tmp/retrieval.json
Compilation is outside the Rust timers. First-use means fresh workspace, not cold OS cache.
"""
import argparse
import json
import pathlib
import platform
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[2]
    command = ["cargo", "test", "--locked", "-p", "dial-tools", "--lib",
               "retrieval_measurement_", "--", "--ignored", "--nocapture", "--test-threads=1"]
    result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, check=False)
    sys.stderr.write(result.stderr)
    if result.returncode:
        sys.stderr.write(result.stdout)
        raise SystemExit(result.returncode)
    records = {"search": [], "graph": []}
    for line in result.stdout.splitlines():
        for marker, key in [("RETRIEVAL_MEASUREMENT ", "search"), ("RETRIEVAL_GRAPH ", "graph")]:
            if marker in line:
                records[key].append(json.loads(line.split(marker, 1)[1]))
    if len(records["search"]) != 20 or len(records["graph"]) != 8:
        raise RuntimeError("Incomplete measurement output: " + result.stdout)
    report = {"schema_version": 1, "platform": platform.platform(),
              "profile": "cargo test default (unoptimized)", "command": command,
              "cache_caveat": "Fresh-workspace is first invocation after fixture writes, not cold OS cache.",
              "buffer_caveat": "Conservative content buffer bound, not measured RSS; excludes witnesses and allocator overhead.",
              "graph_caveat": "Static six-file fixture, heuristic crate references; not Aider or resolved symbols, not model task success.",
              **records}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Verified 20 search cells and 8 ranking cells; results: {args.output}")


if __name__ == "__main__":
    main()
