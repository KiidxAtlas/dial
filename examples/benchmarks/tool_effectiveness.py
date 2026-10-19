#!/usr/bin/env python3
"""Analyze repeated machine benchmarks for tool effectiveness offline; never invokes a model or judge.

Reuses reliability.py for run/contract validation and compare.py for outcome checks. Tool
declarations are an opt-in sidecar keyed by case ID and bound to the verifier contract hash; they
never route tools at runtime. Per-trial evidence comes from the main-agent `tool_trace` that the
benchmark child records. A tool is reported as declared_not_used only when a complete trace shows it
was offered on a successful request and never executed; absent, partial or failed traces stay unknown,
so no missed opportunity is inferred from a missing call.
"""
import argparse
import json
import statistics
import tomllib
from pathlib import Path

from compare import checked_success, invalid_reason
from reliability import group_trials, run_contract, summarize, task_contract

TRACE_KEYS = {"scope", "complete", "successful_requests", "offered", "invoked"}
EXPECTATION_KEYS = {"id", "expected_tools", "opportunity_tools", "contract_sha256"}
MAX_TOOL_NAME_BYTES = 64


def metric(metrics, name):
    value = metrics.get(name)
    return value if type(value) is int and value >= 0 else None


def observed_tokens(metrics):
    if metrics.get("token_usage_observed") is True and not metrics.get("token_usage_incomplete", False):
        return metrics["prompt_tokens"] + metrics["output_tokens"]
    return None


def trial_work(case):
    metrics = case["metrics"]
    return {
        "calls": metric(metrics, "tool_calls"),
        "errors": metric(metrics, "tool_errors"),
        "contract_errors": metric(metrics, "tool_contract_errors"),
        "repairs": metric(metrics, "repair_attempts"),
        "tokens": observed_tokens(metrics),
        "elapsed_ms": metric(metrics, "elapsed_ms"),
    }


def total(values):
    return None if not values or any(value is None for value in values) else sum(values)


def median(values):
    return None if not values or any(value is None for value in values) else statistics.median(values)


def tool_name(value, field):
    if not isinstance(value, str) or not value or len(value.encode()) > MAX_TOOL_NAME_BYTES:
        raise ValueError(f"invalid tool name: {field}")
    return value


def tool_names(value, field):
    if not isinstance(value, list):
        raise ValueError(f"invalid tool list: {field}")
    names = [tool_name(name, field) for name in value]
    if len(names) != len(set(names)):
        raise ValueError(f"duplicate tool names: {field}")
    return names


def count_map(value, field):
    if not isinstance(value, dict):
        raise ValueError(f"tool_trace {field} must be an object")
    result = {}
    for name, count in value.items():
        tool_name(name, field)
        if type(count) is not int or count < 0:
            raise ValueError(f"invalid tool_trace count: {field}.{name}")
        result[name] = count
    return result


def trace(case):
    """Validated main-agent trace for one trial, or None when the report carries no trace."""
    recorded = case["metrics"].get("tool_trace")
    if recorded is None:
        return None
    if not isinstance(recorded, dict) or set(recorded) != TRACE_KEYS:
        raise ValueError("tool_trace requires exactly scope, complete, successful_requests, offered and invoked")
    if recorded["scope"] != "main_agent":
        raise ValueError("tool_trace scope must be main_agent")
    if type(recorded["complete"]) is not bool:
        raise ValueError("tool_trace complete must be boolean")
    requests = recorded["successful_requests"]
    if type(requests) is not int or requests < 0:
        raise ValueError("invalid tool_trace successful_requests")
    return {
        "complete": recorded["complete"],
        "successful_requests": requests,
        "offered": count_map(recorded["offered"], "offered"),
        "invoked": count_map(recorded["invoked"], "invoked"),
    }


def expectation_status(recorded, tool):
    """Observed execution is used; absence needs a complete trace with at least one successful request."""
    if recorded is None:
        return "unknown"
    if recorded["invoked"].get(tool, 0) > 0:
        return "used"
    if not recorded["complete"] or recorded["successful_requests"] == 0:
        return "unknown"
    if recorded["offered"].get(tool, 0) == 0:
        return "not_offered"
    return "declared_not_used"


def load_expectations(payload):
    if not isinstance(payload, dict) or payload.get("version") != 1 or set(payload) - {"version", "cases"}:
        raise ValueError("expectations require version = 1 and only cases")
    cases = payload.get("cases")
    if not isinstance(cases, list):
        raise ValueError("expectations require a cases list")
    result = {}
    for entry in cases:
        if not isinstance(entry, dict) or set(entry) - EXPECTATION_KEYS or "id" not in entry or "contract_sha256" not in entry:
            raise ValueError("expectation entries require id and contract_sha256 and only known keys")
        task = entry["id"]
        if not isinstance(task, str) or not task or task in result:
            raise ValueError(f"invalid or duplicate expectation case: {task!r}")
        digest = entry["contract_sha256"]
        if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
            raise ValueError(f"invalid expectation contract: {task}")
        expected = tool_names(entry.get("expected_tools", []), f"{task}.expected_tools")
        opportunity = tool_names(entry.get("opportunity_tools", []), f"{task}.opportunity_tools")
        if not expected and not opportunity:
            raise ValueError(f"expectation declares no tools: {task}")
        result[task] = {"contract_sha256": digest, "expected_tools": expected, "opportunity_tools": opportunity}
    return result


def tool_outcomes(traces, declaration):
    outcomes = {}
    for field, kind in (("expected_tools", "expected"), ("opportunity_tools", "opportunity")):
        for tool in declaration[field]:
            counts = {"used": 0, "declared_not_used": 0, "not_offered": 0, "unknown": 0}
            for recorded in traces:
                counts[expectation_status(recorded, tool)] += 1
            assessed = counts["used"] + counts["declared_not_used"]
            outcomes[tool] = {"kind": kind, **counts,
                              "declared_use_rate": counts["used"] / assessed if assessed else None}
    return outcomes


def task_summary(task, trials, checked, declaration):
    works = [trial_work(case) for case in trials]
    traces = [trace(case) for case in trials]
    successes = [work for case, work in zip(trials, works) if checked and checked_success(case)]
    errors = [work["errors"] for work in works]
    errors_known = None not in errors
    failed_trials = sum(value > 0 for value in errors) if errors_known else None
    recovered = sum(1 for case, work in zip(trials, works)
                    if checked and checked_success(case) and work["errors"] and work["errors"] > 0) if errors_known else None
    observed = [recorded for recorded in traces if recorded is not None]
    invoked = {}
    for recorded in observed:
        for name, count in recorded["invoked"].items():
            invoked[name] = invoked.get(name, 0) + count
    return {
        "id": task,
        "checked": checked,
        "trials": len(trials),
        "checked_successes": len(successes),
        "all_trials_passed": len(successes) == len(trials) if checked else None,
        "all_trials": {
            "tool_calls": total([w["calls"] for w in works]),
            "tool_errors": total(errors),
            "tool_contract_errors": total([w["contract_errors"] for w in works]),
            "repair_attempts": total([w["repairs"] for w in works]),
            "observed_tokens": total([w["tokens"] for w in works]),
            "elapsed_ms": total([w["elapsed_ms"] for w in works]),
        },
        "per_success": None if not checked else {
            "trials": len(successes),
            "tool_calls_median": median([w["calls"] for w in successes]),
            "tool_errors_median": median([w["errors"] for w in successes]),
            "tool_contract_errors_median": median([w["contract_errors"] for w in successes]),
            "observed_tokens_median": median([w["tokens"] for w in successes]),
            "elapsed_ms_median": median([w["elapsed_ms"] for w in successes]),
        },
        "recovery": {
            "trials_with_tool_errors": failed_trials,
            "recovered_successes": recovered if checked else None,
            "recovery_rate": recovered / failed_trials if checked and failed_trials else None,
        },
        "trace": {
            "trials_with_trace": len(observed),
            "trials_complete": sum(recorded["complete"] for recorded in observed),
            "trials_without_trace": len(trials) - len(observed),
            # Lower bound: executions recorded in trials that carry a trace; absent calls are never counted.
            "invoked_calls_observed": invoked,
        },
        "expectations": tool_outcomes(traces, declaration) if declaration else None,
    }


def summarize_tools(payload, expectations=None):
    expectations = expectations or {}
    report = payload.get("machine_benchmark", payload)
    validated = summarize(payload)
    repetitions = report["policy"]["repetitions"]
    entries = {entry["configuration"]["name"]: entry for entry in report["configurations"]}
    tasks_seen = set()
    configurations = []
    for summary in validated["configurations"]:
        entry = entries[summary["name"]]
        if invalid_reason(entry["report"]):
            raise ValueError(f"invalid run: {summary['name']}")
        groups = group_trials(entry["report"], repetitions)
        tasks = []
        for task, trials_by_index in sorted(groups.items()):
            cases = [trials_by_index[i] for i in range(repetitions)]
            contract = task_contract(cases[0])
            declaration = expectations.get(task)
            if declaration and declaration["contract_sha256"] != contract[2]:
                raise ValueError(f"expectation contract mismatch: {task}")
            tasks_seen.add(task)
            checked = contract[1] > 0 and "answer_requires_manual_review" not in contract[3]
            tasks.append(task_summary(task, cases, checked, declaration))
        configurations.append({"name": summary["name"], "run_contract": run_contract(entry["report"]), "tasks": tasks})
    unknown = sorted(set(expectations) - tasks_seen)
    if unknown:
        raise ValueError(f"expectations name absent cases: {', '.join(unknown)}")
    baseline_name = report["baseline_configuration"]
    baseline = next(item for item in configurations if item["name"] == baseline_name)
    baseline_tasks = {task["id"]: task for task in baseline["tasks"]}
    comparisons = []
    for configuration in configurations:
        by_id = {task["id"]: task for task in configuration["tasks"]}
        matched, excluded = [], []
        for task in configuration["tasks"]:
            before = baseline_tasks[task["id"]]
            if task["all_trials_passed"] is True and before["all_trials_passed"] is True:
                matched.append(task["id"])
            else:
                excluded.append(task["id"])
        regressed = [task["id"] for task in configuration["tasks"] if task["checked"]
                     and task["checked_successes"] < baseline_tasks[task["id"]]["checked_successes"]]
        ratios = {}
        for field in ("tool_calls_median", "tool_errors_median", "observed_tokens_median", "elapsed_ms_median"):
            before_total = total([baseline_tasks[name]["per_success"][field] for name in matched])
            after_total = total([by_id[name]["per_success"][field] for name in matched])
            ratios[field] = after_total / before_total if before_total else None
        comparisons.append({
            "name": configuration["name"],
            "matched_success_tasks": matched,
            "excluded_tasks": excluded,
            "observed_regressed_tasks": regressed,
            "observed_correctness_nonregressing": not regressed,
            "matched_success_ratios": ratios,
        })
    return {
        "kind": "tool-effectiveness",
        "scope": "main_agent",
        "suite": entries[baseline_name]["report"]["suite"],
        "repetitions": repetitions,
        "baseline_configuration": baseline_name,
        "configurations": configurations,
        "comparisons": comparisons,
        "limitations": "Recorded outcome checks and main-agent traces only; no model calls and no new correctness proof. Worker, extension and compaction-internal calls are outside the trace. Declared tools are opt-in human expectations: declared_not_used means the tool was offered on a successful request and never executed, not a missed opportunity. Absent, partial or failed traces are unknown. Offers do not imply connection or execute permission. Per-success medians use only checked successful trials; unknown usage or counters stay null. Matched comparisons use tasks passing every trial in both configurations. No quota, cost or statistical significance is inferred; compare real-model runs separately from scripted harness regressions.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="machine benchmark JSON, direct or CLI envelope")
    parser.add_argument("--expectations", type=Path, help="opt-in TOML sidecar of declared tool expectations")
    parser.add_argument("--output", type=Path, help="new summary file; existing files are never overwritten")
    args = parser.parse_args()
    try:
        expectations = None
        if args.expectations:
            expectations = load_expectations(tomllib.loads(args.expectations.read_text()))
        result = summarize_tools(json.loads(args.report.read_text()), expectations)
        text = json.dumps(result, indent=2, allow_nan=False) + "\n"
        if args.output:
            with args.output.open("x") as output:
                output.write(text)
    except (ValueError, KeyError, TypeError, OSError, tomllib.TOMLDecodeError) as error:
        parser.error(str(error))
    print(text, end="")
    return int(any(not entry["observed_correctness_nonregressing"] for entry in result["comparisons"]))


if __name__ == "__main__":
    raise SystemExit(main())
