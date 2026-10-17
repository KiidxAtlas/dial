#!/usr/bin/env python3
"""Analyze repeated Dial machine benchmarks offline; never invokes a model or a judge."""
import argparse
import json
import math
import statistics
from pathlib import Path

from compare import checked_success, evidence, indexed, invalid_reason


def wilson_interval(successes, trials):
    """95% Wilson interval for repeated Bernoulli trials of one fixed task."""
    if not trials:
        return None
    z = 1.959963984540054
    rate = successes / trials
    denominator = 1 + z * z / trials
    middle = (rate + z * z / (2 * trials)) / denominator
    radius = z * math.sqrt(rate * (1 - rate) / trials + z * z / (4 * trials * trials)) / denominator
    return [max(0.0, middle - radius), min(1.0, middle + radius)]


def task_contract(case):
    recorded = evidence(case)
    if not recorded:
        raise ValueError("reliability analysis requires outcome-check evidence")
    return (case["checks_total"], recorded["outcome_checks"],
            recorded["contract_sha256"], recorded.get("categories", []))


def run_contract(report):
    contract = report.get("contract")
    if not isinstance(contract, dict) or contract.get("schema_version") != 1:
        raise ValueError("reliability analysis requires a version-1 run contract")
    for name in ("suite_sha256", "source_sha256", "executable_sha256", "configuration_sha256"):
        value = contract.get(name)
        if not isinstance(value, str) or len(value) != 64 or any(c not in "0123456789abcdef" for c in value):
            raise ValueError(f"invalid run contract: {name}")
    return contract


def summarize_configuration(entry, repetitions):
    report = entry["report"]
    reason = invalid_reason(report)
    if reason:
        raise ValueError(f"invalid run for {entry['configuration']['name']}: {reason}")
    run_contract(report)
    groups = {}
    for key, case in indexed(report).items():
        trial, separator, task = key.partition(":")
        if not separator or not task or trial not in {str(i) for i in range(repetitions)}:
            raise ValueError(f"case lacks a valid repetition prefix: {key}")
        task_contract(case)
        groups.setdefault(task, {})[int(trial)] = case
    contracts = {}
    tasks = []
    for task, trials in sorted(groups.items()):
        if set(trials) != set(range(repetitions)):
            raise ValueError(f"missing repetitions for task: {task}")
        cases = list(trials.values())
        contract = task_contract(cases[0])
        if any(task_contract(case) != contract for case in cases):
            raise ValueError(f"verification contract changed across trials: {task}")
        contracts[task] = contract
        eligible = contract[1] > 0 and "answer_requires_manual_review" not in contract[3]
        successes = sum(checked_success(case) for case in cases)
        complete_usage = all(case["metrics"].get("token_usage_observed") is True
                             and not case["metrics"].get("token_usage_incomplete", False) for case in cases)
        repairs = [case["metrics"].get("repair_attempts") for case in cases]
        if any(value is not None and (type(value) is not int or value < 0) for value in repairs):
            raise ValueError(f"invalid repair attempts: {task}")
        successful_ms = [case["metrics"]["elapsed_ms"] for case in cases if checked_success(case)]
        tasks.append({
            "id": task,
            "checked": eligible,
            "trials": repetitions,
            "checked_successes": successes,
            "checked_success_rate": successes / repetitions if eligible else None,
            "success_rate_wilson_95": wilson_interval(successes, repetitions) if eligible else None,
            "all_trials_passed": successes == repetitions if eligible else None,
            "any_trial_passed": successes > 0 if eligible else None,
            "unavailable_check_trials": sum(bool(evidence(case)["unavailable_check_indices"]) for case in cases),
            "agent_error_trials": sum(case["metrics"].get("agent_error") is True for case in cases),
            "all_trial_median_ms": statistics.median(case["metrics"]["elapsed_ms"] for case in cases),
            "successful_trial_median_ms": statistics.median(successful_ms) if successful_ms else None,
            "observed_tokens": sum(case["metrics"]["prompt_tokens"] + case["metrics"]["output_tokens"] for case in cases) if complete_usage else None,
            "tool_calls": sum(case["metrics"]["tool_calls"] for case in cases),
            "tool_errors": sum(case["metrics"]["tool_errors"] for case in cases),
            "repair_attempts": sum(repairs) if all(value is not None for value in repairs) else None,
        })
    checked = [task for task in tasks if task["checked"]]
    return {
        "name": entry["configuration"]["name"],
        "execution_build": report.get("execution_build"),
        "run_contract": report["contract"],
        "checked_tasks": len(checked),
        "excluded_tasks": [task["id"] for task in tasks if not task["checked"]],
        "checked_trial_success_rate": sum(task["checked_successes"] for task in checked) / (len(checked) * repetitions) if checked else None,
        "observed_all_trials_passed_fraction": sum(task["all_trials_passed"] for task in checked) / len(checked) if checked else None,
        "observed_any_trial_passed_fraction": sum(task["any_trial_passed"] for task in checked) / len(checked) if checked else None,
        "tasks": tasks,
    }, contracts


def summarize(payload):
    report = payload.get("machine_benchmark", payload)
    repetitions = report["policy"]["repetitions"]
    if type(repetitions) is not int or not 1 <= repetitions <= 10:
        raise ValueError("machine repetitions must be 1..10")
    entries = report["configurations"]
    if not isinstance(entries, list) or not entries:
        raise ValueError("expected nonempty machine configurations")
    summaries = []
    names = set()
    reference = None
    for entry in entries:
        name = entry["configuration"]["name"]
        if not isinstance(name, str) or not name or name in names:
            raise ValueError("configuration names must be nonempty and unique")
        names.add(name)
        summary, contracts = summarize_configuration(entry, repetitions)
        contract = summary["run_contract"]
        identity = (entry["report"]["suite"], contract["suite_sha256"], contract["source_sha256"], contracts)
        if reference is not None and identity != reference:
            raise ValueError("configurations must have identical task, source and verification contracts")
        reference = identity
        summaries.append(summary)
    baseline_name = report["baseline_configuration"]
    baseline = next((entry for entry in summaries if entry["name"] == baseline_name), None)
    if baseline is None:
        raise ValueError("baseline configuration is absent")
    baseline_tasks = {task["id"]: task for task in baseline["tasks"]}
    comparisons = []
    for entry in summaries:
        regressions = [task["id"] for task in entry["tasks"] if task["checked"]
                       and task["checked_successes"] < baseline_tasks[task["id"]]["checked_successes"]]
        matched = [task for task in entry["tasks"] if task["all_trials_passed"] is True
                   and baseline_tasks[task["id"]]["all_trials_passed"] is True]
        before_ms = sum(baseline_tasks[task["id"]]["successful_trial_median_ms"] for task in matched)
        after_ms = sum(task["successful_trial_median_ms"] for task in matched)
        comparisons.append({
            "name": entry["name"],
            "observed_regressed_tasks": regressions,
            "matched_consistent_tasks": [task["id"] for task in matched],
            "matched_consistent_latency_ratio": after_ms / before_ms if before_ms else None,
            "observed_correctness_nonregressing": not regressions if entry["checked_tasks"] else None,
        })
    return {
        "kind": "repeated-benchmark-reliability",
        "suite": entries[0]["report"]["suite"],
        "repetitions": repetitions,
        "baseline_configuration": baseline_name,
        "configurations": summaries,
        "comparisons": comparisons,
        "limitations": "Recorded outcome checks only; no model calls or new correctness proof. All/any-trial fractions are empirical task counts, not estimates of independent pass^k/pass@k. Per-task Wilson intervals assume independent trials; few repetitions leave substantial uncertainty. Unavailable checks and agent errors remain in the denominator. Unknown usage is null. Compare real-model measurements separately from scripted harness regressions; inspect traces and manually assess output quality.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="machine benchmark JSON, direct or CLI envelope")
    parser.add_argument("--output", type=Path, help="new summary file; existing files are never overwritten")
    args = parser.parse_args()
    try:
        result = summarize(json.loads(args.report.read_text()))
        text = json.dumps(result, indent=2, allow_nan=False) + "\n"
        if args.output:
            with args.output.open("x") as output:
                output.write(text)
    except (ValueError, KeyError, TypeError, OSError) as error:
        parser.error(str(error))
    print(text, end="")
    return int(any(entry["observed_correctness_nonregressing"] is False for entry in result["comparisons"]))


if __name__ == "__main__":
    raise SystemExit(main())
