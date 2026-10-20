"""Offline reporting regressions, not measurements of model quality."""
import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from reliability import pass_at_k, pass_hat_k, summarize
from test_compare import report


def machine(outcomes=(True, True, True), candidate=None):
    def configuration(name, results, elapsed):
        cases = []
        for trial, passed in enumerate(results):
            case = report(passed, elapsed)["cases"][0]
            case["id"] = f"{trial}:repair"
            case["metrics"].update(token_usage_observed=True, repair_attempts=int(not passed))
            cases.append(case)
        return {"configuration": {"name": name}, "report": {
            "suite": "coding", "cases": cases,
            "contract": {"schema_version": 1, "suite_sha256": "a" * 64,
                         "source_sha256": "b" * 64, "executable_sha256": "c" * 64,
                         "configuration_sha256": ("d" if name == "baseline" else "e") * 64},
        }}
    entries = [configuration("baseline", outcomes, 100)]
    if candidate is not None:
        entries.append(configuration("candidate", candidate, 50))
    return {"policy": {"repetitions": len(outcomes)},
            "baseline_configuration": "baseline", "configurations": entries}


def summarize_input_cases(payload):
    return payload["configurations"][0]["report"]["cases"]


class ReliabilityTests(unittest.TestCase):
    def test_pass_estimators_match_unbiased_definitions(self):
        # n=5, c=3: pass@2 = 1 - C(2,2)/C(5,2); pass^2 = C(3,2)/C(5,2).
        self.assertAlmostEqual(pass_at_k(5, 3, 2), 1 - 1 / 10)
        self.assertAlmostEqual(pass_hat_k(5, 3, 2), 3 / 10)
        self.assertAlmostEqual(pass_at_k(5, 3, 1), 3 / 5)
        self.assertAlmostEqual(pass_hat_k(5, 3, 1), 3 / 5)
        self.assertEqual(pass_at_k(5, 3, 3), 1)  # n - c < k: some draw must pass
        self.assertEqual(pass_hat_k(5, 3, 4), 0)  # c < k: no all-pass draw exists
        self.assertEqual((pass_at_k(4, 0, 2), pass_hat_k(4, 4, 4)), (0, 1))
        for invalid in [(3, 1, 0), (3, 1, 4), (3, 4, 1), (3, -1, 1)]:
            with self.assertRaises(ValueError):
                pass_at_k(*invalid)
            with self.assertRaises(ValueError):
                pass_hat_k(*invalid)

    def test_pass_estimators_are_unbiased_against_exact_expectation(self):
        # E[estimator] over Binomial(n, p) successes equals 1-(1-p)^k and p^k.
        from math import comb
        n, p = 6, 0.3
        weights = [comb(n, c) * p ** c * (1 - p) ** (n - c) for c in range(n + 1)]
        for k in range(1, n + 1):
            any_mean = sum(w * pass_at_k(n, c, k) for c, w in enumerate(weights))
            all_mean = sum(w * pass_hat_k(n, c, k) for c, w in enumerate(weights))
            self.assertAlmostEqual(any_mean, 1 - (1 - p) ** k)
            self.assertAlmostEqual(all_mean, p ** k)

    def test_consistency_curves_are_reported_per_task_and_overall(self):
        payload = machine((True, False, True))
        payload["configurations"][0]["report"]["cases"] += [
            dict(copy.deepcopy(case), id=case["id"].replace("repair", "other"))
            for case in summarize_input_cases(machine((True, True, True)))]
        configuration = summarize(payload)["configurations"][0]
        repair, other = sorted(configuration["tasks"], key=lambda task: task["id"])[::-1]
        self.assertEqual(repair["id"], "repair")
        self.assertEqual(set(repair["pass_at_k"]), {"1", "2", "3"})
        self.assertAlmostEqual(repair["pass_at_k"]["2"], 1)
        self.assertAlmostEqual(repair["pass_hat_k"]["2"], 1 / 3)
        self.assertEqual(repair["pass_hat_k"]["3"], 0)
        self.assertEqual(other["pass_hat_k"]["3"], 1)
        self.assertAlmostEqual(configuration["pass_at_k"]["1"], configuration["checked_trial_success_rate"])
        self.assertAlmostEqual(configuration["pass_hat_k"]["2"], (1 / 3 + 1) / 2)
        self.assertEqual(configuration["pass_hat_k"]["3"], configuration["observed_all_trials_passed_fraction"])
        self.assertEqual(configuration["pass_at_k"]["3"], configuration["observed_any_trial_passed_fraction"])

    def test_unchecked_tasks_have_no_consistency_curves(self):
        payload = machine()
        for case in payload["configurations"][0]["report"]["cases"]:
            case["evidence"].update(categories=["informational"], outcome_checks=0)
        configuration = summarize(payload)["configurations"][0]
        self.assertIsNone(configuration["tasks"][0]["pass_at_k"])
        self.assertIsNone(configuration["tasks"][0]["pass_hat_k"])
        self.assertIsNone(configuration["pass_at_k"])
        self.assertIsNone(configuration["pass_hat_k"])

    def test_repeated_success_measures_consistency_not_best_of_n(self):
        result = summarize(machine((True, False, True)))
        configuration = result["configurations"][0]
        task = configuration["tasks"][0]
        self.assertEqual(configuration["checked_trial_success_rate"], 2 / 3)
        self.assertEqual(configuration["observed_any_trial_passed_fraction"], 1)
        self.assertEqual(configuration["observed_all_trials_passed_fraction"], 0)
        self.assertEqual(task["repair_attempts"], 1)
        self.assertEqual(task["observed_tokens"], 90)
        lower, upper = task["success_rate_wilson_95"]
        self.assertLess(lower, 2 / 3)
        self.assertGreater(upper, 2 / 3)

    def test_fast_failure_cannot_be_presented_as_latency_win(self):
        result = summarize(machine(candidate=(True, False, True)))
        comparison = result["comparisons"][1]
        self.assertEqual(comparison["observed_regressed_tasks"], ["repair"])
        self.assertFalse(comparison["observed_correctness_nonregressing"])
        self.assertIsNone(comparison["matched_consistent_latency_ratio"])
        consistent = summarize(machine(candidate=(True, True, True)))
        self.assertEqual(consistent["comparisons"][1]["matched_consistent_latency_ratio"], 0.5)

    def test_infrastructure_failures_remain_visible_and_in_denominator(self):
        payload = machine((True, False, True))
        case = payload["configurations"][0]["report"]["cases"][1]
        case["evidence"]["unavailable_check_indices"] = [0]
        case["metrics"].update(agent_error=True, token_usage_incomplete=True)
        task = summarize(payload)["configurations"][0]["tasks"][0]
        self.assertEqual(task["checked_success_rate"], 2 / 3)
        self.assertEqual(task["unavailable_check_trials"], 1)
        self.assertEqual(task["agent_error_trials"], 1)
        self.assertIsNone(task["observed_tokens"])

    def test_metric_only_and_manual_tasks_cannot_establish_correctness(self):
        for categories, count in [(["informational"], 0), (["answer_requires_manual_review"], 1)]:
            payload = machine()
            for case in payload["configurations"][0]["report"]["cases"]:
                case["evidence"].update(categories=categories, outcome_checks=count)
            result = summarize(payload)
            configuration = result["configurations"][0]
            self.assertEqual(configuration["checked_tasks"], 0)
            self.assertIsNone(configuration["checked_trial_success_rate"])
            self.assertIsNone(result["comparisons"][0]["observed_correctness_nonregressing"])

    def test_missing_duplicate_and_out_of_range_trials_are_rejected(self):
        for mutate in [lambda cases: cases.pop(),
                       lambda cases: cases.append(copy.deepcopy(cases[0])),
                       lambda cases: cases[0].update(id="3:repair"),
                       lambda cases: cases[0].update(id="00:repair")]:
            payload = machine()
            mutate(payload["configurations"][0]["report"]["cases"])
            with self.assertRaises(ValueError):
                summarize(payload)

    def test_changed_task_source_or_verifier_contracts_are_rejected(self):
        for field in ("source_sha256", "suite_sha256"):
            payload = machine(candidate=(True, True, True))
            payload["configurations"][1]["report"]["contract"][field] = "f" * 64
            with self.assertRaisesRegex(ValueError, "identical task, source"):
                summarize(payload)
        payload = machine()
        payload["configurations"][0]["report"]["cases"][1]["evidence"]["contract_sha256"] = "f" * 64
        with self.assertRaisesRegex(ValueError, "contract changed across trials"):
            summarize(payload)

    def test_invalid_runs_and_legacy_unverified_reports_are_rejected(self):
        payload = machine()
        payload["configurations"][0]["report"]["invalid_run"] = "missing credentials"
        with self.assertRaisesRegex(ValueError, "invalid run"):
            summarize(payload)
        payload = machine()
        del payload["configurations"][0]["report"]["cases"][0]["evidence"]
        with self.assertRaisesRegex(ValueError, "outcome-check evidence"):
            summarize(payload)

    def test_envelopes_and_task_ids_containing_colons_are_supported(self):
        payload = machine()
        for case in payload["configurations"][0]["report"]["cases"]:
            case["id"] += ":boundary"
        result = summarize({"machine_benchmark": payload})
        self.assertEqual(result["configurations"][0]["tasks"][0]["id"], "repair:boundary")

    def test_cli_reports_regression_and_refuses_to_overwrite_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "observed.json"
            output = Path(temporary) / "summary.json"
            source.write_text(json.dumps({"machine_benchmark": machine(candidate=(True, False, True))}))
            command = [sys.executable, str(Path(__file__).with_name("reliability.py")), str(source), "--output", str(output)]
            completed = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(completed.returncode, 1, completed.stderr)
            self.assertEqual(json.loads(output.read_text())["comparisons"][1]["observed_regressed_tasks"], ["repair"])
            before = output.read_bytes()
            rejected = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(rejected.returncode, 2)
            self.assertEqual(output.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
