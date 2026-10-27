#!/usr/bin/env python3
"""Compare Dial benchmark reports; report analysis is not new model-quality proof."""
import argparse
import json
import statistics
from pathlib import Path


def direct_report(payload, configuration=None):
    if not isinstance(payload, dict):
        raise ValueError("expected a benchmark report object")
    report = payload.get("report", payload)
    if not isinstance(report, dict):
        raise ValueError("expected a benchmark report object")
    machine = report.get("machine_benchmark", report)
    if not isinstance(machine, dict):
        raise ValueError("expected a machine benchmark report object")
    if "configurations" not in machine:
        if configuration is not None:
            raise ValueError("configuration selection requires a machine benchmark report")
        return report
    entries = machine["configurations"]
    if not isinstance(entries, list) or not entries:
        raise ValueError("expected nonempty machine configurations")
    if any(not isinstance(entry, dict) or not isinstance(entry.get("configuration"), dict)
           or not isinstance(entry.get("report"), dict) for entry in entries):
        raise ValueError("expected machine configuration and report objects")
    names = [entry["configuration"]["name"] for entry in entries]
    if any(not isinstance(name, str) or not name for name in names) or len(names) != len(set(names)):
        raise ValueError("configuration names must be nonempty and unique")
    if configuration is None:
        if len(entries) != 1:
            raise ValueError("select a machine configuration explicitly; available: " + ", ".join(names))
        return entries[0]["report"]
    if configuration not in names:
        raise ValueError(f"unknown machine configuration: {configuration}; available: " + ", ".join(names))
    return entries[names.index(configuration)]["report"]


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


def run_contract(report):
    """Validate recorded execution identity; configuration/build may vary between runs."""
    contract = report.get("contract")
    if not isinstance(contract, dict) or type(contract.get("schema_version")) is not int or contract["schema_version"] != 1:
        raise ValueError("analysis requires a version-1 run contract")
    for name in ("suite_sha256", "source_sha256", "executable_sha256", "configuration_sha256"):
        value = contract.get(name)
        if not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
            raise ValueError(f"invalid run contract: {name}")
    return contract


def invalid_reason(report):
    # Mirrors BenchmarkReport::invalid_reason: a run whose every case failed before any observed
    # model activity never measured the model, so its zero score is not a result.
    if report.get("invalid_run"):
        return report["invalid_run"]
    cases = report.get("cases")
    if not isinstance(cases, list) or not cases:
        return None
    def inert(metrics):
        if not isinstance(metrics, dict):
            raise ValueError("case metrics must be an object")
        usage = metrics.get("runtime_usage")
        if usage is None:
            usage = {}
        if not isinstance(usage, dict):
            raise ValueError("runtime_usage must be an object or null")
        all_usage = usage.get("all", {})
        if not isinstance(all_usage, dict):
            raise ValueError("runtime_usage.all must be an object")
        attempts = all_usage.get("transport_attempts", 0)
        if type(attempts) is not int or not 0 <= attempts <= 2**64 - 1:
            raise ValueError("runtime_usage.all.transport_attempts must be an unsigned 64-bit integer")
        return metrics.get("agent_error") is True and attempts == 0 and not metrics.get("token_usage_observed") and not metrics.get("token_usage_incomplete") and all(not metrics.get(name) for name in ACTIVITY_METRICS)
    # Validate every case, even if an earlier active case makes the run non-inert.
    activity = [inert(case.get("metrics", {})) for case in cases]
    if all(activity):
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


def compare(baseline, candidate, baseline_configuration=None, candidate_configuration=None):
    baseline = direct_report(baseline, baseline_configuration)
    candidate = direct_report(candidate, candidate_configuration)
    for name, report in (("baseline", baseline), ("candidate", candidate)):
        reason = invalid_reason(report)
        if reason:
            raise ValueError(f"{name} report is an invalid run and cannot be compared: {reason}")
    contracts = [run_contract(report) if report.get("contract") is not None else None
                 for report in (baseline, candidate)]
    if all(contract is not None for contract in contracts):
        if any(contracts[0][field] != contracts[1][field]
               for field in ("schema_version", "suite_sha256", "source_sha256")):
            raise ValueError("run contracts must have identical schema, suite and source identities")
        run_identity = "matched"
    else:
        run_identity = "legacy_unknown"
    before, after = indexed(baseline), indexed(candidate)
    if baseline["suite"] != candidate["suite"] or before.keys() != after.keys():
        raise ValueError("reports must contain the same suite and case IDs")
    if any(before[key]["checks_total"] != after[key]["checks_total"] or evidence(before[key]).get("outcome_checks") != evidence(after[key]).get("outcome_checks") or evidence(before[key]).get("contract_sha256") != evidence(after[key]).get("contract_sha256") or evidence(before[key]).get("categories", before[key].get("categories", [])) != evidence(after[key]).get("categories", after[key].get("categories", [])) for key in before):
        raise ValueError("case verification contracts differ")
    regressions = [key for key in before if checked_success(before[key]) and not checked_success(after[key])]
    matched = [key for key in before if checked_success(before[key]) and checked_success(after[key])]
    before_ms = sum(before[key]["metrics"]["elapsed_ms"] for key in matched)
    after_ms = sum(after[key]["metrics"]["elapsed_ms"] for key in matched)
    return {
        "kind": "benchmark-report-comparison",
        "suite": baseline["suite"],
        "run_identity": run_identity,
        "baseline_run_contract": contracts[0],
        "candidate_run_contract": contracts[1],
        "baseline_build": baseline.get("execution_build"),
        "candidate_build": candidate.get("execution_build"),
        "baseline": totals(before),
        "candidate": totals(after),
        "regressed_cases": regressions,
        "matched_successful_cases": matched,
        "matched_success_latency_ratio": after_ms / before_ms if before_ms else None,
        "correctness_nonregressing": not regressions if any(checked_success(case) for case in before.values()) else None,
        "limitations": "Uses recorded checks, not new proof. Legacy reports without outcome-check metadata remain readable but cannot establish checked coding success. Use identical fixture versions, models and declared verifiers; compare real-model runs separately from scripted runs. Unknown token usage is null, not free. Metric-only and manually reviewed checks do not establish model quality.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--baseline-configuration", help="Configuration name in the baseline machine report")
    parser.add_argument("--candidate-configuration", help="Configuration name in the candidate machine report")
    args = parser.parse_args()
    try:
        report = compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()),
                         args.baseline_configuration, args.candidate_configuration)
    except (ValueError, KeyError, TypeError, OSError) as error:
        parser.error(str(error))
    text = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(text)
    print(text, end="")
    if report["correctness_nonregressing"] is None:
        return 3
    return 0 if report["correctness_nonregressing"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
