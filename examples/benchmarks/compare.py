#!/usr/bin/env python3
"""Compare direct Dial benchmark reports; report analysis is not new model-quality proof."""
import argparse
import json
import statistics
from pathlib import Path


def direct_report(payload):
    if not isinstance(payload, dict):
        raise ValueError("expected a benchmark report object")
    return payload.get("report", payload)


def evidence(case):
    return case.get("evidence") or {}


def indexed(report):
    cases = report["cases"]
    if not isinstance(cases, list) or not cases:
        raise ValueError("comparison requires nonempty benchmark cases")
    result = {}
    for case in cases:
        key = case["id"]
        if key in result:
            raise ValueError(f"duplicate case: {key}")
        for name in ("checks_total", "checks_passed"):
            if type(case[name]) is not int or case[name] < 0:
                raise ValueError(f"invalid {name}: {key}")
        if type(case["passed"]) is not bool or case["checks_passed"] > case["checks_total"]:
            raise ValueError(f"invalid outcome: {key}")
        if case["passed"] and case["checks_passed"] != case["checks_total"]:
            raise ValueError(f"passing outcome hides failed checks: {key}")
        for name in ("elapsed_ms", "prompt_tokens", "output_tokens", "tool_calls", "tool_errors"):
            if type(case["metrics"][name]) is not int or case["metrics"][name] < 0:
                raise ValueError(f"invalid metric {name}: {key}")
        recorded = evidence(case)
        if recorded:
            count = recorded["outcome_checks"]
            if type(count) is not int or not 0 <= count <= case["checks_total"]:
                raise ValueError(f"invalid outcome checks: {key}")
            for field in ("failed_check_indices", "unavailable_check_indices"):
                indices = recorded[field]
                if not isinstance(indices, list) or any(type(i) is not int or not 0 <= i < case["checks_total"] for i in indices) or len(indices) != len(set(indices)):
                    raise ValueError(f"invalid {field}: {key}")
            if len(recorded["failed_check_indices"]) != case["checks_total"] - case["checks_passed"] or not set(recorded["unavailable_check_indices"]).issubset(recorded["failed_check_indices"]):
                raise ValueError(f"verification evidence disagrees: {key}")
            contract = recorded["contract_sha256"]
            if not isinstance(contract, str) or len(contract) != 64 or any(c not in "0123456789abcdef" for c in contract):
                raise ValueError(f"invalid verification contract: {key}")
        result[key] = case
    return result


ACTIVITY_METRICS = ("prompt_tokens", "output_tokens", "cached_tokens", "reasoning_tokens", "tool_calls", "compactions", "turns_completed")


def invalid_reason(report):
    # Mirrors BenchmarkReport::invalid_reason: a run whose every case failed before any observed
    # model activity never measured the model, so its zero score is not a result.
    if report.get("invalid_run"):
        return report["invalid_run"]
    cases = report.get("cases")
    if not isinstance(cases, list) or not cases:
        return None
    def inert(metrics):
        return metrics.get("agent_error") is True and not metrics.get("token_usage_observed") and not metrics.get("token_usage_incomplete") and all(not metrics.get(name) for name in ACTIVITY_METRICS)
    if all(inert(case.get("metrics") or {}) for case in cases):
        return f"all {len(cases)} cases failed before any observed model activity"
    return None


def checked_success(case):
    # Informational cases explicitly requiring manual review cannot establish correctness.
    recorded = evidence(case)
    categories = recorded.get("categories", case.get("categories", []))
    outcome_checks = recorded.get("outcome_checks", 0)
    return case["passed"] and outcome_checks > 0 and "answer_requires_manual_review" not in categories


def totals(cases):
    successes = [case for case in cases.values() if checked_success(case)]
    elapsed = [case["metrics"]["elapsed_ms"] for case in successes]
    return {
        "checked_successes": len(successes),
        "total_cases": len(cases),
        "successful_case_median_ms": statistics.median(elapsed) if elapsed else None,
        "tokens": sum(case["metrics"]["prompt_tokens"] + case["metrics"]["output_tokens"] for case in cases.values()) if all(case["metrics"].get("token_usage_observed") is True and not case["metrics"].get("token_usage_incomplete", False) for case in cases.values()) else None,
        "tool_calls": sum(case["metrics"]["tool_calls"] for case in cases.values()),
        "tool_errors": sum(case["metrics"]["tool_errors"] for case in cases.values()),
    }


def compare(baseline, candidate):
    baseline, candidate = direct_report(baseline), direct_report(candidate)
    for name, report in (("baseline", baseline), ("candidate", candidate)):
        reason = invalid_reason(report)
        if reason:
            raise ValueError(f"{name} report is an invalid run and cannot be compared: {reason}")
    before, after = indexed(baseline), indexed(candidate)
    if baseline["suite"] != candidate["suite"] or before.keys() != after.keys():
        raise ValueError("reports must contain the same suite and case IDs")
    if any(before[key]["checks_total"] != after[key]["checks_total"] or evidence(before[key]).get("contract_sha256") != evidence(after[key]).get("contract_sha256") or evidence(before[key]).get("categories", before[key].get("categories", [])) != evidence(after[key]).get("categories", after[key].get("categories", [])) for key in before):
        raise ValueError("case verification contracts differ")
    regressions = [key for key in before if checked_success(before[key]) and not checked_success(after[key])]
    matched = [key for key in before if checked_success(before[key]) and checked_success(after[key])]
    before_ms = sum(before[key]["metrics"]["elapsed_ms"] for key in matched)
    after_ms = sum(after[key]["metrics"]["elapsed_ms"] for key in matched)
    return {
        "kind": "benchmark-report-comparison",
        "suite": baseline["suite"],
        "baseline_build": baseline.get("execution_build"),
        "candidate_build": candidate.get("execution_build"),
        "baseline": totals(before),
        "candidate": totals(after),
        "regressed_cases": regressions,
        "matched_successful_cases": matched,
        "matched_success_latency_ratio": after_ms / before_ms if before_ms else None,
        "correctness_nonregressing": not regressions,
        "limitations": "Uses recorded checks, not new proof. Legacy reports without outcome-check metadata remain readable but cannot establish checked coding success. Use identical fixture versions, models and declared verifiers; compare real-model runs separately from scripted runs. Unknown token usage is null, not free. Metric-only and manually reviewed checks do not establish model quality.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        report = compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()))
    except (ValueError, KeyError, TypeError, OSError) as error:
        parser.error(str(error))
    text = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(text)
    print(text, end="")
    return 0 if report["correctness_nonregressing"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
