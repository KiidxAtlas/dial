#!/usr/bin/env python3
"""Grade main-agent routing in one discovery-routing direct report. Never runs a model."""
import argparse
import json
from pathlib import Path

from compare import checked_success, direct_report, indexed, invalid_reason
from tool_effectiveness import trace

PREFERRED = {
    "transcript-preview-path": {"find", "glob"},
    "bounded-total-owner": {"find"},
    "literal-label-source": {"grep"},
    "focused-preview-test": {"verify"},
    "build-preview-catalog": {"bash"},
}


def grade(payload):
    report = direct_report(payload)
    if reason := invalid_reason(report):
        raise ValueError(reason)
    cases = indexed(report)
    if report.get("suite") != "discovery-routing-v1" or cases.keys() != PREFERRED.keys():
        raise ValueError("expected all five cases of a direct discovery-routing-v1 report")
    rows = []
    for key, preferred in PREFERRED.items():
        case = cases[key]
        observed = trace(case)
        route = "unknown"
        if observed and observed["complete"] and observed["successful_requests"]:
            used = {name for name, count in observed["invoked"].items() if count}
            matched = bool(used & preferred) and ("bash" in preferred or "bash" not in used)
            route = "preferred" if matched else "nonpreferred"
        rows.append({"id": key, "checked_success": checked_success(case), "route": route,
                     "invoked": observed["invoked"] if observed else None})
    return rows


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    try:
        rows = grade(json.loads(args.report.read_text()))
    except (KeyError, ValueError, OSError) as exc:
        parser.error(str(exc))
    print(json.dumps(rows, indent=2))
    raise SystemExit(not all(row["checked_success"] and row["route"] == "preferred" for row in rows))
