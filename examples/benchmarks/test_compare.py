import copy
import json
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest

from compare import compare


def report(passed=True, elapsed=100, categories=None):
    return {"suite": "coding", "cases": [{
        "id": "repair", "passed": passed,
        "checks_total": 1, "checks_passed": int(passed),
        "categories": categories or ["debugging"],
        "evidence": {"categories": categories or ["debugging"], "outcome_checks": 1, "failed_check_indices": [] if passed else [0], "unavailable_check_indices": [], "contract_sha256": "a" * 64},
        "metrics": {"elapsed_ms": elapsed, "prompt_tokens": 20, "output_tokens": 10, "tool_calls": 3, "tool_errors": 0},
    }]}


class ComparisonTests(unittest.TestCase):
    def test_fast_failure_is_a_correctness_regression_not_a_speed_win(self):
        result = compare(report(), report(False, 1))
        self.assertFalse(result["correctness_nonregressing"])
        self.assertEqual(result["regressed_cases"], ["repair"])
        self.assertIsNone(result["matched_success_latency_ratio"])

    def test_latency_compares_only_the_same_successful_cases(self):
        result = compare(report(elapsed=100), report(elapsed=50))
        self.assertEqual(result["matched_success_latency_ratio"], 0.5)
        self.assertTrue(result["correctness_nonregressing"])
        self.assertIsNone(compare(report(elapsed=0), report(elapsed=0))["matched_success_latency_ratio"])

    def test_manual_review_and_empty_checks_do_not_establish_success(self):
        manual = report(categories=["answer_requires_manual_review"])
        self.assertEqual(compare(manual, manual)["candidate"]["checked_successes"], 0)
        empty = report()
        empty["cases"][0].update(checks_total=0, checks_passed=0)
        empty["cases"][0]["evidence"]["outcome_checks"] = 0
        self.assertEqual(compare(empty, empty)["candidate"]["checked_successes"], 0)
        self.assertIsNone(compare(manual, manual)["correctness_nonregressing"])
        self.assertIsNone(compare(empty, empty)["correctness_nonregressing"])

    def test_no_baseline_success_is_inconclusive_even_if_candidate_improves(self):
        result = compare(report(False), report())
        self.assertIsNone(result["correctness_nonregressing"])
        self.assertEqual(result["regressed_cases"], [])
        self.assertIsNone(result["matched_success_latency_ratio"])

    def test_cli_exit_codes_and_json_distinguish_inconclusive(self):
        script = Path(__file__).with_name("compare.py")
        with tempfile.TemporaryDirectory() as directory:
            before = Path(directory) / "before.json"
            after = Path(directory) / "after.json"
            output = Path(directory) / "comparison.json"
            for baseline, candidate, code, correctness in (
                (report(), report(), 0, True),
                (report(), report(False), 1, False),
                (report(False), report(), 3, None),
            ):
                with self.subTest(code=code):
                    before.write_text(json.dumps(baseline))
                    after.write_text(json.dumps(candidate))
                    process = subprocess.run(
                        [sys.executable, str(script), str(before), str(after), "--output", str(output)],
                        capture_output=True, text=True, check=False)
                    self.assertEqual(process.returncode, code, process.stderr)
                    self.assertIs(json.loads(process.stdout)["correctness_nonregressing"], correctness)
                    self.assertEqual(json.loads(output.read_text()), json.loads(process.stdout))
            before.write_text("not json")
            process = subprocess.run([sys.executable, str(script), str(before), str(after)],
                                     capture_output=True, text=True, check=False)
            self.assertEqual(process.returncode, 2)

    def test_mismatched_tasks_duplicate_cases_and_fabricated_passes_are_rejected(self):
        baseline = report()
        for field, value in [("id", "different"), ("checks_total", 2), ("checks_passed", 0)]:
            changed = copy.deepcopy(baseline)
            changed["cases"][0][field] = value
            with self.assertRaises(ValueError):
                compare(baseline, changed)
        duplicate = copy.deepcopy(baseline)
        duplicate["cases"].append(copy.deepcopy(duplicate["cases"][0]))
        with self.assertRaises(ValueError):
            compare(baseline, duplicate)


    def test_invalid_runs_are_refused_including_legacy_reports(self):
        flagged = report(False)
        flagged["invalid_run"] = "invalid run: all 1 cases failed before any observed model activity"
        with self.assertRaisesRegex(ValueError, "baseline report is an invalid run"):
            compare(flagged, report())
        legacy = report(False, 16)
        legacy["cases"][0]["metrics"].update(prompt_tokens=0, output_tokens=0, tool_calls=0, agent_error=True, turns_completed=0)
        with self.assertRaisesRegex(ValueError, "candidate report is an invalid run"):
            compare(report(), legacy)
        active = copy.deepcopy(legacy)
        active["cases"][0]["metrics"]["tool_calls"] = 1
        self.assertFalse(compare(report(), active)["correctness_nonregressing"])

    def test_cli_report_envelopes_are_supported(self):
        result = compare({"report": report()}, {"report": report()})
        self.assertTrue(result["correctness_nonregressing"])

    def test_machine_reports_require_unambiguous_selection(self):
        machine = {"machine_benchmark": {"configurations": [
            {"configuration": {"name": "raw"}, "report": report(elapsed=100)},
            {"configuration": {"name": "dial"}, "report": report(elapsed=50)},
        ]}}
        with self.assertRaisesRegex(ValueError, "select a machine configuration"):
            compare(machine, machine)
        result = compare(machine, machine, "raw", "dial")
        self.assertEqual(result["matched_success_latency_ratio"], 0.5)
        with self.assertRaisesRegex(ValueError, "unknown machine configuration"):
            compare(machine, machine, "missing", "dial")
        with self.assertRaisesRegex(ValueError, "requires a machine benchmark"):
            compare(report(), report(), "raw")
        machine["machine_benchmark"]["configurations"].pop()
        self.assertTrue(compare(machine, machine)["correctness_nonregressing"])
        machine["machine_benchmark"]["configurations"] *= 2
        with self.assertRaisesRegex(ValueError, "unique"):
            compare(machine, machine, "raw", "raw")

    def test_outcome_check_counts_are_part_of_the_verification_contract(self):
        candidate = report()
        candidate["cases"][0]["evidence"]["outcome_checks"] = 0
        with self.assertRaisesRegex(ValueError, "verification contracts differ"):
            compare(report(), candidate)

    def test_malformed_report_envelopes_are_rejected(self):
        for payload in ({"report": None}, {"machine_benchmark": None},
                        {"configurations": [None]}, {"configurations": []}):
            with self.subTest(payload=payload), self.assertRaises(ValueError):
                compare(payload, report())

    def test_metric_only_cases_are_not_coding_successes(self):
        measured = report()
        measured["cases"][0]["evidence"] = {
            "categories": ["informational"], "outcome_checks": 0,
            "failed_check_indices": [], "unavailable_check_indices": [],
            "contract_sha256": "a" * 64,
        }
        self.assertEqual(compare(measured, measured)["candidate"]["checked_successes"], 0)

    def test_unobserved_usage_is_unknown_not_free(self):
        measured = report()
        measured["cases"][0]["metrics"]["token_usage_observed"] = False
        self.assertIsNone(compare(measured, measured)["candidate"]["tokens"])

    def test_changed_verification_contract_is_rejected_even_with_same_counts(self):
        baseline = report()
        baseline["cases"][0]["evidence"] = {
            "categories": ["debugging"], "outcome_checks": 1,
            "failed_check_indices": [], "unavailable_check_indices": [],
            "contract_sha256": "a" * 64,
        }
        candidate = copy.deepcopy(baseline)
        candidate["cases"][0]["evidence"]["contract_sha256"] = "b" * 64
        with self.assertRaises(ValueError):
            compare(baseline, candidate)

    def test_legacy_reports_do_not_establish_checked_success(self):
        legacy = report()
        del legacy["cases"][0]["evidence"]
        self.assertEqual(compare(legacy, legacy)["candidate"]["checked_successes"], 0)
        self.assertIsNone(compare(legacy, legacy)["correctness_nonregressing"])

    def test_invalid_failure_evidence_is_rejected(self):
        measured = report()
        measured["cases"][0]["evidence"]["failed_check_indices"] = [0]
        with self.assertRaises(ValueError):
            compare(measured, measured)

    def test_known_complete_usage_retains_actual_counts(self):
        measured = report()
        measured["cases"][0]["metrics"].update(token_usage_observed=True, token_usage_incomplete=False)
        self.assertEqual(compare(measured, measured)["candidate"]["tokens"], 30)
        measured["cases"][0]["metrics"]["token_usage_incomplete"] = True
        self.assertIsNone(compare(measured, measured)["candidate"]["tokens"])

    def test_run_identity_requires_matching_source_and_suite(self):
        baseline = report()
        baseline["contract"] = {
            "schema_version": 1, "suite_sha256": "a" * 64,
            "source_sha256": "b" * 64, "executable_sha256": "c" * 64,
            "configuration_sha256": "d" * 64,
        }
        self.assertEqual(compare(baseline, baseline)["run_identity"], "matched")
        for field, changed in (("source_sha256", "e" * 64),
                               ("suite_sha256", "e" * 64), ("schema_version", 2)):
            candidate = copy.deepcopy(baseline)
            candidate["contract"][field] = changed
            with self.subTest(field=field), self.assertRaises(ValueError):
                compare(baseline, candidate)
        candidate = copy.deepcopy(baseline)
        candidate["contract"].update(executable_sha256="e" * 64, configuration_sha256="f" * 64)
        self.assertEqual(compare(baseline, candidate)["run_identity"], "matched")
        self.assertEqual(compare(report(), baseline)["run_identity"], "legacy_unknown")
        self.assertEqual(compare(report(), report())["run_identity"], "legacy_unknown")

    def test_zero_token_transport_failure_is_observed_activity(self):
        failed = report(False)
        failed["cases"][0]["metrics"].update(
            prompt_tokens=0, output_tokens=0, tool_calls=0, agent_error=True,
            runtime_usage={"all": {"transport_attempts": 1}})
        self.assertFalse(compare(report(), failed)["correctness_nonregressing"])
        failed["cases"][0]["metrics"]["runtime_usage"]["all"]["transport_attempts"] = 0
        with self.assertRaisesRegex(ValueError, "invalid run"):
            compare(report(), failed)

    def test_malformed_transport_usage_is_rejected_as_invalid_report(self):
        for usage in [False, True, 4, "", "usage", [], [1], {"all": None}, {"all": True}, {"all": [1]},
                      {"all": {"transport_attempts": True}},
                      {"all": {"transport_attempts": -1}},
                      {"all": {"transport_attempts": 2**64}},
                      {"all": {"transport_attempts": "1"}}]:
            with self.subTest(usage=usage):
                malformed = report()
                malformed["cases"][0]["metrics"]["runtime_usage"] = usage
                with self.assertRaises(ValueError):
                    compare(malformed, report())
                # An active first case must not short-circuit validation of later records.
                multiple = report()
                malformed["cases"][0]["id"] = "second"
                multiple["cases"].extend(malformed["cases"])
                with self.assertRaises(ValueError):
                    compare(multiple, multiple)

    def test_optional_transport_usage_preserves_legacy_reports(self):
        for usage in [None, {}, {"all": {}}, {"all": {"transport_attempts": 0}},
                      {"all": {"transport_attempts": 2**64 - 1}}]:
            with self.subTest(usage=usage):
                valid = report()
                valid["cases"][0]["metrics"]["runtime_usage"] = usage
                self.assertTrue(compare(valid, valid)["correctness_nonregressing"])


if __name__ == "__main__":
    unittest.main()
