#!/usr/bin/env python3
"""Summarize the serialized whole-tree view of benchmark reports offline; never invokes a model.

Each case's `metrics.whole_execution` is written by the benchmark child (see `RunMetrics` in
crates/dial-app/src/benchmark.rs). A total is the sum over cases that report that field as known,
and it is null when no case knows it. Cases without the view (legacy reports), with null fields, or
with a `complete` flag whose prerequisites are unknown stay counted as unknown or partial, never zero.
Dispatches are admitted-to-execution counts (main admissions plus worker tool starts), not successful
effects. Worker elapsed time overlaps parent wall time and is reported separately, never added to it.
"""
import argparse
import json
from pathlib import Path


DISPATCH_FIELDS = ("parent_tool_dispatches", "worker_tool_dispatches", "tool_dispatches")
REQUEST_FIELDS = ("parent_requests", "worker_requests")
SUM_FIELDS = DISPATCH_FIELDS + REQUEST_FIELDS + ("worker_elapsed_ms",)
# A case is complete only when its own `complete` flag is true AND these totals are known, so a
# flag alone (or a malformed field) never certifies the whole tree.
COMPLETENESS_FIELDS = ("tool_dispatches", "parent_requests", "worker_requests")


def known_count(value):
    return value if type(value) is int and value >= 0 else None


def summarize(report):
    cases = report.get("cases")
    if not isinstance(cases, list):
        raise ValueError("report must contain a cases list")
    totals = {field: 0 for field in SUM_FIELDS}
    known = {field: 0 for field in SUM_FIELDS}
    complete = 0
    viewed = 0
    for case in cases:
        metrics = case.get("metrics") if isinstance(case, dict) else None
        whole = metrics.get("whole_execution") if isinstance(metrics, dict) else None
        if not isinstance(whole, dict):
            continue
        viewed += 1
        values = {field: known_count(whole.get(field)) for field in SUM_FIELDS}
        for field, value in values.items():
            if value is not None:
                totals[field] += value
                known[field] += 1
        if whole.get("complete") is True and all(
            values[field] is not None for field in COMPLETENESS_FIELDS
        ):
            complete += 1
    return {
        "cases": len(cases),
        "cases_without_view": len(cases) - viewed,
        "complete_cases": complete,
        "partial_cases": viewed - complete,
        # Null, never zero, for any field no case reported as known.
        "totals": {field: totals[field] if known[field] else None for field in SUM_FIELDS},
        "known_cases": known,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    print(json.dumps(summarize(json.loads(args.report.read_text())), indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
