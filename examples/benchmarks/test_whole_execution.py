"""Offline whole-execution summary regressions: legacy, partial and complete reports stay distinct."""
import unittest

from whole_execution import summarize


def case(whole=None):
    metrics = {"prompt_tokens": 1}
    if whole is not None:
        metrics["whole_execution"] = whole
    return {"metrics": metrics}


COMPLETE = {
    "parent_tool_dispatches": 4,
    "worker_tool_dispatches": 5,
    "tool_dispatches": 9,
    "parent_requests": 2,
    "worker_requests": 4,
    "worker_elapsed_ms": 900,
    "complete": True,
}
PARTIAL = {
    "parent_tool_dispatches": 1,
    "worker_tool_dispatches": None,
    "tool_dispatches": None,
    "parent_requests": None,
    "worker_requests": None,
    "worker_elapsed_ms": None,
    "complete": False,
}


class WholeExecutionSummaryTests(unittest.TestCase):
    def test_legacy_cases_without_the_view_are_unknown_not_zero(self):
        summary = summarize({"cases": [case(), {"metrics": None}]})
        self.assertEqual(summary["cases_without_view"], 2)
        self.assertEqual(summary["complete_cases"], 0)
        self.assertEqual(summary["partial_cases"], 0)
        self.assertEqual(set(summary["totals"].values()), {None})
        self.assertEqual(set(summary["known_cases"].values()), {0})

    def test_known_fields_sum_once_and_partial_fields_are_excluded(self):
        summary = summarize({"cases": [case(COMPLETE), case(PARTIAL)]})
        self.assertEqual(summary["complete_cases"], 1)
        self.assertEqual(summary["partial_cases"], 1)
        self.assertEqual(summary["totals"]["tool_dispatches"], 9)
        self.assertEqual(summary["known_cases"]["tool_dispatches"], 1)
        self.assertEqual(summary["totals"]["parent_tool_dispatches"], 5)
        self.assertEqual(summary["known_cases"]["parent_tool_dispatches"], 2)
        self.assertEqual(summary["totals"]["worker_requests"], 4)
        self.assertEqual(summary["known_cases"]["worker_requests"], 1)

    def test_a_field_no_case_knows_is_null_not_zero(self):
        summary = summarize({"cases": [case(PARTIAL)]})
        self.assertIsNone(summary["totals"]["worker_tool_dispatches"])
        self.assertEqual(summary["known_cases"]["worker_tool_dispatches"], 0)
        self.assertIsNone(summary["totals"]["parent_requests"])
        self.assertEqual(summary["totals"]["parent_tool_dispatches"], 1)

    def test_malformed_counts_are_unknown_and_block_completeness(self):
        bad = dict(COMPLETE, worker_tool_dispatches=True, parent_requests=-1)
        summary = summarize({"cases": [case(bad)]})
        self.assertIsNone(summary["totals"]["worker_tool_dispatches"])
        self.assertEqual(summary["known_cases"]["worker_tool_dispatches"], 0)
        self.assertIsNone(summary["totals"]["parent_requests"])
        self.assertEqual(summary["known_cases"]["tool_dispatches"], 1)
        self.assertEqual(summary["complete_cases"], 0)
        self.assertEqual(summary["partial_cases"], 1)

    def test_complete_flag_requires_its_prerequisite_totals(self):
        flagged = dict(COMPLETE, tool_dispatches=None)
        summary = summarize({"cases": [case(flagged)]})
        self.assertEqual(summary["complete_cases"], 0)
        self.assertEqual(summary["partial_cases"], 1)

    def test_report_without_cases_is_rejected(self):
        with self.assertRaises(ValueError):
            summarize({})


if __name__ == "__main__":
    unittest.main()
