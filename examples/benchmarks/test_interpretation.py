"""Offline fixture/oracle and executable grader tests; not live model/tool quality tests."""
import copy
import csv
import io
import json
from decimal import Decimal
from pathlib import Path
import shutil
import tempfile
import tomllib
import unittest
import xml.etree.ElementTree as ET
from zipfile import ZipFile

from generate_interpretation import fixture_bytes
from interpretation_cases import CASES, suite_text
from test_quality import checks_pass

ROOT = Path(__file__).resolve().parent
FIXTURE = ROOT / "interpretation-fixture"
EXPECTED = {case_id: answer for case_id, _, answer, _ in CASES}


def rows(name):
    with (FIXTURE / name).open(newline="") as stream:
        return list(csv.DictReader(stream))


class InterpretationFixtureTests(unittest.TestCase):
    def test_committed_inputs_and_suite_are_reproducible(self):
        first, second = fixture_bytes(), fixture_bytes()
        self.assertEqual(first, second)
        for name, content in first.items():
            with self.subTest(name=name):
                self.assertEqual((FIXTURE / name).read_bytes(), content)
        self.assertEqual((ROOT / "interpretation.toml").read_text(), suite_text())

    def test_requirements_distinguish_formatting_from_substantive_change(self):
        def requirements(name):
            return {line.removeprefix("- ").split(": ", 1)[0]: line.removeprefix("- ").split(": ", 1)[1]
                    for line in (FIXTURE / name).read_text().splitlines() if ": " in line}
        before = requirements("requirements-v1.md")
        formatting = requirements("requirements-format.md")
        revised = requirements("requirements-v2.md")
        self.assertNotEqual((FIXTURE / "requirements-v1.md").read_bytes(),
                            (FIXTURE / "requirements-format.md").read_bytes())
        self.assertEqual(before, formatting)
        self.assertEqual([key for key in before if before[key] != revised[key]],
                         EXPECTED["requirements-revisions"]["substantive_changed_ids"])
        self.assertIn("30 days", before["R1"])
        self.assertIn("90 days", revised["R1"])

    def test_outlier_is_beyond_preview_and_full_coverage_changes_result(self):
        data = rows("measurements.csv")
        values = [int(row["value"]) for row in data]
        maximum = max(data, key=lambda row: int(row["value"]))
        self.assertGreater(data.index(maximum), 100)
        self.assertNotEqual(max(values[:100]), max(values))
        self.assertEqual({"rows_examined": len(data), "sum": sum(values),
                          "maximum": max(values), "maximum_id": maximum["id"]},
                         EXPECTED["full-dataset-outlier"])

    def test_duration_conversion_reverses_raw_numeric_ranking(self):
        data = rows("durations.csv")
        factors = {"ms": Decimal(1), "s": Decimal(1000), "min": Decimal(60000), "h": Decimal(3600000)}
        normalized = {row["id"]: Decimal(row["duration"]) * factors[row["unit"]] for row in data}
        answer = {"total_ms": int(sum(normalized.values())),
                  "fastest_id": min(normalized, key=normalized.get),
                  "slowest_id": max(normalized, key=normalized.get)}
        self.assertEqual(answer, EXPECTED["mixed-time-units"])
        self.assertNotEqual(max(data, key=lambda row: Decimal(row["duration"]))["id"], answer["slowest_id"])

    def test_money_requires_exact_decimals(self):
        data = rows("money.csv")
        total = sum(Decimal(row["amount"]) for row in data)
        self.assertEqual(format(total, ".2f"), EXPECTED["exact-decimal-money"]["total"])
        self.assertEqual(len(data), EXPECTED["exact-decimal-money"]["rows_examined"])
        self.assertNotEqual(format(sum(float(row["amount"]) for row in data), ".2f"), format(total, ".2f"))

    def test_opposing_quotes_have_no_deciding_authority(self):
        answer = EXPECTED["contradictory-quotes"]
        for quote in answer["quotes"]:
            self.assertIn(quote["quote"], (FIXTURE / quote["source"]).read_text().splitlines())
        self.assertIsNone(json.loads((FIXTURE / "source-register.json").read_text())["authority_order"])
        self.assertIsNone(answer["approved"])
        self.assertNotEqual(answer["quotes"][0]["quote"], answer["quotes"][1]["quote"])

    def test_nested_records_and_missing_jsonl_duration(self):
        nested = json.loads((FIXTURE / "nested.json").read_text())
        orders = [order for batch in nested["batches"] for order in batch["orders"]]
        quantity = sum(line["quantity"] for order in orders for line in order["lines"] if line["sku"] == "X")
        events = [json.loads(line) for line in (FIXTURE / "events.jsonl").read_text().splitlines()]
        present = [event for event in events if event["payload"]["duration"] is not None]
        total = sum(Decimal(str(event["payload"]["duration"]["value"])) *
                    {"ms": 1, "s": 1000}[event["payload"]["duration"]["unit"]] for event in present)
        self.assertEqual({"x_quantity": quantity, "orders_examined": len(orders), "events_examined": len(events),
                          "events_with_duration": len(present), "total_duration_ms": int(total),
                          "missing_duration_ids": [event["event_id"] for event in events if event not in present]},
                         EXPECTED["nested-json-jsonl"])

    def test_real_xlsx_package_contains_unverified_inconsistent_cache(self):
        ns = {"s": "http://schemas.openxmlformats.org/spreadsheetml/2006/main"}
        with ZipFile(FIXTURE / "formula-cache.xlsx") as archive:
            sheet = ET.fromstring(archive.read("xl/worksheets/sheet1.xml"))
            workbook = ET.fromstring(archive.read("xl/workbook.xml"))
        cells = {cell.attrib["r"]: cell for cell in sheet.findall(".//s:c", ns)}
        answer = EXPECTED["spreadsheet-cache-boundary"]
        self.assertEqual(cells["A3"].find("s:f", ns).text, answer["formula"])
        self.assertEqual(cells["A3"].find("s:v", ns).text, answer["cached_value"])
        computed = sum(int(cells[key].find("s:v", ns).text) for key in ["A1", "A2"])
        self.assertEqual(str(computed), answer["independently_computed_value"])
        self.assertNotEqual(str(computed), answer["cached_value"])
        self.assertEqual(workbook.find("s:calcPr", ns).attrib["calcMode"], "manual")
        self.assertFalse(answer["cache_verified"])

    def test_audio_and_scan_inputs_explicitly_stop_at_double_boundaries(self):
        audio = json.loads((FIXTURE / "transcript-double.json").read_text())
        self.assertFalse(audio["recording_available"])
        self.assertFalse(audio["live_audio_processed"])
        self.assertEqual(audio["segments"][0]["alternatives"], EXPECTED["ambiguous-transcription-double"]["alternatives"])
        scan = json.loads((FIXTURE / "scan-boundary.json").read_text())
        self.assertFalse(scan["scan_available"])
        self.assertFalse(scan["ocr_output_available"])
        self.assertEqual(scan["pages"][0]["text_extraction"], "")
        self.assertIsNone(EXPECTED["scanned-document-boundary"]["page_is_empty"])


class InterpretationGraderTests(unittest.TestCase):
    def test_native_checks_reject_missing_wrong_partial_and_overconfident_answers(self):
        suite = tomllib.loads((ROOT / "interpretation.toml").read_text())
        self.assertEqual({case["id"] for case in suite["cases"]}, set(EXPECTED))
        for case in suite["cases"]:
            with self.subTest(case=case["id"]), tempfile.TemporaryDirectory() as temporary:
                workspace = Path(temporary) / "project"
                shutil.copytree(FIXTURE, workspace)
                path = workspace / "answer.json"
                self.assertFalse(checks_pass(case, workspace))
                expected = EXPECTED[case["id"]]
                # Deliberate oracle answer validates the grader, not any model's capabilities.
                path.write_text(json.dumps(dict(reversed(list(expected.items()))), indent=2))
                self.assertTrue(checks_pass(case, workspace))
                for key, value in expected.items():
                    wrong = copy.deepcopy(expected)
                    wrong[key] = (not value) if isinstance(value, bool) else "incorrect"
                    path.write_text(json.dumps(wrong))
                    with self.subTest(field=key):
                        self.assertFalse(checks_pass(case, workspace))
                partial = dict(expected)
                partial.pop(next(iter(partial)))
                path.write_text(json.dumps(partial))
                self.assertFalse(checks_pass(case, workspace))
                path.write_text("not json")
                self.assertFalse(checks_pass(case, workspace))


if __name__ == "__main__":
    unittest.main()
