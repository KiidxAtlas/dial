"""Offline tool-effectiveness regressions: fixture acceptance and analyzer contracts, not model quality."""
import copy
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

from test_quality import checks_pass
from tool_effectiveness import load_expectations, summarize_tools

ROOT = pathlib.Path(__file__).resolve().parent
FIXTURE = ROOT / "tool-effectiveness-fixture"
SUITE = ROOT / "tool-effectiveness.toml"


def repair_transaction_name(text):
    return re.sub(r"\btransaction_total\b", "ledger_total", text)


REPAIRS = {
    "selection-find-function": lambda workspace: (workspace / "selection.txt").write_bytes(b"bounded_transaction_total"),
    "composition-rename-consumers": lambda workspace: [
        (workspace / name).write_text(repair_transaction_name((workspace / name).read_text()))
        for name in ("ledger.py", "statement.py")],
    "requirement-boundary-floor": lambda workspace: (workspace / "ledger.py").write_text(
        (workspace / "ledger.py").read_text().replace(
            "return min(transaction_total(rows), LIMIT_CENTS)",
            "return max(0, min(transaction_total(rows), LIMIT_CENTS))")),
    "long-context-current-code": lambda workspace: (workspace / "report-code.txt").write_bytes(b"CURRENT-7741"),
    "diagnosis-environment-not-code": lambda workspace: (workspace / "diagnosis.txt").write_bytes(b"environment"),
}


def suite_cases():
    return tomllib.loads(SUITE.read_text())["cases"]


def tool_trace(*, complete=True, requests=1, offered=(), invoked=None):
    """Main-agent trace as the benchmark child reports it in case metrics."""
    return {
        "scope": "main_agent",
        "complete": complete,
        "successful_requests": requests,
        "offered": {name: requests for name in offered},
        "invoked": dict(invoked or {}),
    }


def trial_case(trial, task, passed=True, *, calls=3, errors=0, elapsed=100, observed=True,
               contract="a" * 64, trace=None):
    case = {
        "id": f"{trial}:{task}", "passed": passed, "checks_total": 1, "checks_passed": int(passed),
        "categories": ["debugging"],
        "evidence": {"categories": ["debugging"], "outcome_checks": 1,
                     "failed_check_indices": [] if passed else [0], "unavailable_check_indices": [],
                     "contract_sha256": contract},
        "metrics": {"elapsed_ms": elapsed, "prompt_tokens": 20, "output_tokens": 10,
                    "tool_calls": calls, "tool_errors": errors, "tool_contract_errors": 0,
                    "token_usage_observed": observed, "token_usage_incomplete": not observed,
                    "repair_attempts": int(not passed)},
    }
    if trace is not None:
        case["metrics"]["tool_trace"] = trace
    return case


def machine(configs, *, repetitions=None):
    """configs: {name: [case dict per trial]}; the first configuration is the baseline."""
    repetitions = repetitions or len(next(iter(configs.values())))
    entries = []
    for name, cases in configs.items():
        entries.append({"configuration": {"name": name}, "report": {
            "suite": "tools", "cases": cases,
            "contract": {"schema_version": 1, "suite_sha256": "a" * 64, "source_sha256": "b" * 64,
                         "executable_sha256": "c" * 64,
                         "configuration_sha256": name.encode().hex().ljust(64, "0")[:64]}}})
    return {"policy": {"repetitions": repetitions}, "baseline_configuration": next(iter(configs)),
            "configurations": entries}


def task_cases(outcomes, task="fix", traces=None, **kwargs):
    traces = traces or [None] * len(outcomes)
    return [trial_case(trial, task, passed, trace=traces[trial], **kwargs)
            for trial, passed in enumerate(outcomes)]


def expectations(task="fix", contract="a" * 64, expected=("find",), opportunity=()):
    return load_expectations({"version": 1, "cases": [
        {"id": task, "contract_sha256": contract, "expected_tools": list(expected),
         "opportunity_tools": list(opportunity)}]})


def one_task(payload, declaration=None):
    return summarize_tools(payload, declaration)["configurations"][0]["tasks"][0]


class FixtureAcceptanceTests(unittest.TestCase):
    def test_suite_is_independent_outcome_only(self):
        for case in suite_cases():
            with self.subTest(case=case["id"]):
                self.assertTrue(case["checks"])
                for check in case["checks"]:
                    self.assertIn(check["kind"], {"file_equals", "command"})
                    if check["kind"] == "command":
                        self.assertNotIn("tool", check["command"].lower())

    def test_original_fixtures_fail_and_intended_repairs_pass(self):
        suite = {case["id"]: case for case in suite_cases()}
        self.assertEqual(set(suite), set(REPAIRS))
        for case_id, repair in REPAIRS.items():
            with self.subTest(case=case_id), tempfile.TemporaryDirectory() as temporary:
                workspace = pathlib.Path(temporary) / "project"
                shutil.copytree(FIXTURE, workspace, ignore=shutil.ignore_patterns("__pycache__"))
                self.assertFalse(checks_pass(suite[case_id], workspace))
                repair(workspace)
                self.assertTrue(checks_pass(suite[case_id], workspace))


class MetricTests(unittest.TestCase):
    def test_per_success_medians_ignore_failed_trials_and_all_trials_sum_everything(self):
        payload = machine({"baseline": task_cases((True, False, True))})
        payload["configurations"][0]["report"]["cases"][1]["metrics"]["tool_calls"] = 9
        result = one_task(payload)
        self.assertEqual(result["per_success"]["tool_calls_median"], 3)
        self.assertEqual(result["all_trials"]["tool_calls"], 15)
        self.assertEqual(result["all_trials"]["repair_attempts"], 1)

    def test_recovery_counts_successes_among_trials_with_observed_tool_errors(self):
        cases = [trial_case(0, "fix", True, errors=2), trial_case(1, "fix", False, errors=1),
                 trial_case(2, "fix", True, errors=0)]
        result = one_task(machine({"baseline": cases}))
        self.assertEqual(result["recovery"], {"trials_with_tool_errors": 2, "recovered_successes": 1,
                                              "recovery_rate": 0.5})

    def test_unknown_usage_and_missing_counters_stay_null(self):
        cases = task_cases((True, True, True), observed=False)
        for case in cases:
            del case["metrics"]["tool_contract_errors"]
        result = one_task(machine({"baseline": cases}))
        self.assertIsNone(result["per_success"]["observed_tokens_median"])
        self.assertIsNone(result["all_trials"]["observed_tokens"])
        self.assertIsNone(result["all_trials"]["tool_contract_errors"])
        self.assertIsNone(result["per_success"]["tool_contract_errors_median"])
        self.assertEqual(result["all_trials"]["tool_calls"], 9)


class TraceTests(unittest.TestCase):
    def test_reports_without_traces_stay_unknown_and_are_counted_as_such(self):
        payload = machine({"baseline": task_cases((True, True, True))})
        result = one_task(payload, expectations())
        self.assertEqual(result["trace"]["trials_without_trace"], 3)
        counts = result["expectations"]["find"]
        self.assertEqual((counts["kind"], counts["unknown"], counts["declared_not_used"]), ("expected", 3, 0))
        self.assertIsNone(counts["declared_use_rate"])

    def test_every_trace_status_is_derived_from_observed_offers_and_executions(self):
        traces = [
            tool_trace(offered=["find"], invoked={"find": 1}),                  # used
            tool_trace(offered=["find"]),                                       # declared_not_used
            tool_trace(offered=["read"]),                                       # not_offered
            tool_trace(complete=False, offered=["find"]),                       # unknown: incomplete
            tool_trace(requests=0),                                             # unknown: no successful request
            tool_trace(complete=False, offered=["read"], invoked={"find": 1}),  # used: observed execution
        ]
        cases = [trial_case(i, "fix", True, trace=trace) for i, trace in enumerate(traces)]
        counts = one_task(machine({"baseline": cases}), expectations())["expectations"]["find"]
        self.assertEqual({k: counts[k] for k in ("used", "declared_not_used", "not_offered", "unknown")},
                         {"used": 2, "declared_not_used": 1, "not_offered": 1, "unknown": 2})
        self.assertAlmostEqual(counts["declared_use_rate"], 2 / 3)

    def test_incomplete_traces_never_produce_declared_non_use(self):
        cases = task_cases((True, False, True), traces=[
            tool_trace(complete=False, offered=["find"]),
            tool_trace(complete=False, offered=["find"]),
            tool_trace(complete=False, offered=["find"]),
        ])
        counts = one_task(machine({"baseline": cases}), expectations())["expectations"]["find"]
        self.assertEqual(counts["declared_not_used"], 0)
        self.assertEqual(counts["unknown"], 3)

    def test_opportunities_are_declared_not_used_not_missed(self):
        cases = task_cases((True,), traces=[tool_trace(offered=["tool_search"])])
        declaration = expectations(expected=(), opportunity=("tool_search",))
        result = summarize_tools(machine({"baseline": cases}), declaration)
        counts = result["configurations"][0]["tasks"][0]["expectations"]["tool_search"]
        self.assertEqual((counts["kind"], counts["declared_not_used"]), ("opportunity", 1))
        self.assertNotIn('"missed"', json.dumps(result))
        self.assertNotIn('"eligible"', json.dumps(result))

    def test_cumulative_multi_turn_trace_is_reported_per_trial_without_double_counting(self):
        cases = task_cases((True,), traces=[tool_trace(requests=3, offered=["read"], invoked={"read": 2})])
        task = one_task(machine({"baseline": cases}))
        self.assertEqual(task["trace"]["invoked_calls_observed"], {"read": 2})
        self.assertEqual(task["trace"]["trials_complete"], 1)

    def test_malformed_traces_are_rejected(self):
        valid = tool_trace(offered=["find"])
        bad = [
            {k: v for k, v in valid.items() if k != "complete"},
            dict(valid, extra=True),
            dict(valid, scope="worker"),
            dict(valid, complete="yes"),
            dict(valid, successful_requests=-1),
            dict(valid, offered={"find": -1}),
            dict(valid, invoked={"": 1}),
            dict(valid, invoked=["find"]),
        ]
        for trace in bad:
            with self.subTest(trace=trace), self.assertRaises(ValueError):
                summarize_tools(machine({"baseline": [trial_case(0, "fix", trace=trace)]}), expectations())

    def test_expectation_contract_mismatch_is_rejected(self):
        payload = machine({"baseline": task_cases((True, True, True))})
        with self.assertRaisesRegex(ValueError, "contract mismatch"):
            summarize_tools(payload, expectations(contract="f" * 64))

    def test_expectation_for_absent_case_is_rejected(self):
        payload = machine({"baseline": task_cases((True, True, True))})
        with self.assertRaisesRegex(ValueError, "absent cases"):
            summarize_tools(payload, expectations(task="other"))

    def test_malformed_expectation_sidecars_are_rejected(self):
        valid = {"id": "fix", "contract_sha256": "a" * 64, "expected_tools": ["find"]}
        bad = [{"version": 2, "cases": [valid]},
               {"version": 1, "cases": [valid, dict(valid)]},
               {"version": 1, "cases": [dict(valid, extra=True)]},
               {"version": 1, "cases": [dict(valid, contract_sha256="xyz")]},
               {"version": 1, "cases": [dict(valid, expected_tools=[])]}]
        for payload in bad:
            with self.subTest(payload=payload), self.assertRaises(ValueError):
                load_expectations(payload)


class ComparisonTests(unittest.TestCase):
    def test_matched_success_comparison_excludes_regressed_and_uncertain_tasks(self):
        baseline = machine({"baseline": task_cases((True, True, True), calls=6, elapsed=200)})
        baseline["configurations"].append({"configuration": {"name": "candidate"}, "report": copy.deepcopy(
            baseline["configurations"][0]["report"])})
        candidate = baseline["configurations"][1]["report"]["cases"]
        for case in candidate:
            case["metrics"].update(tool_calls=3, elapsed_ms=100)
        comparison = summarize_tools(baseline)["comparisons"][1]
        self.assertEqual(comparison["matched_success_tasks"], ["fix"])
        self.assertEqual(comparison["matched_success_ratios"]["tool_calls_median"], 0.5)
        self.assertTrue(comparison["observed_correctness_nonregressing"])

        for case in candidate:
            case["passed"] = False
            case["checks_passed"] = 0
            case["evidence"]["failed_check_indices"] = [0]
        regressed = summarize_tools(baseline)["comparisons"][1]
        self.assertEqual(regressed["observed_regressed_tasks"], ["fix"])
        self.assertFalse(regressed["observed_correctness_nonregressing"])
        self.assertEqual(regressed["matched_success_tasks"], [])
        self.assertIsNone(regressed["matched_success_ratios"]["tool_calls_median"])

    def test_invalid_run_and_mixed_contracts_are_refused(self):
        payload = machine({"baseline": task_cases((True, True, True))})
        payload["configurations"][0]["report"]["invalid_run"] = "missing credentials"
        with self.assertRaisesRegex(ValueError, "invalid run"):
            summarize_tools(payload)
        payload = machine({"baseline": task_cases((True, True, True)),
                           "candidate": task_cases((True, True, True))})
        payload["configurations"][1]["report"]["contract"]["source_sha256"] = "f" * 64
        with self.assertRaisesRegex(ValueError, "identical task, source"):
            summarize_tools(payload)

    def test_cli_writes_new_summary_only_and_exposes_regression_status(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = pathlib.Path(temporary) / "observed.json"
            sidecar = pathlib.Path(temporary) / "expect.toml"
            output = pathlib.Path(temporary) / "summary.json"
            sidecar.write_text('version = 1\n[[cases]]\nid = "fix"\ncontract_sha256 = "' + "a" * 64 + '"\nexpected_tools = ["find"]\n')
            payload = machine({"baseline": task_cases((True, True, True)),
                               "candidate": task_cases((True, False, True))})
            source.write_text(json.dumps({"machine_benchmark": payload}))
            command = [sys.executable, str(ROOT / "tool_effectiveness.py"), str(source),
                       "--expectations", str(sidecar), "--output", str(output)]
            completed = subprocess.run(command, capture_output=True, text=True, timeout=20)
            self.assertEqual(completed.returncode, 1, completed.stderr)
            self.assertEqual(json.loads(output.read_text())["comparisons"][1]["observed_regressed_tasks"], ["fix"])
            before = output.read_bytes()
            rejected = subprocess.run(command, capture_output=True, text=True, timeout=20)
            self.assertEqual(rejected.returncode, 2)
            self.assertEqual(output.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
