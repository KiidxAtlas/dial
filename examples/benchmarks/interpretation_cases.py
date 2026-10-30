"""Answer contracts for synthetic interpretation evaluation, outside the copied project.

These contracts grade observable structured answers, not tool choices or prose. They are
not evidence that a model, OCR engine, spreadsheet calculator or transcriber was run.
"""
import json


CASES = [
    ("requirements-revisions",
     "Compare requirements-v1.md against requirements-format.md and requirements-v2.md. Classify each revision and report changed requirement IDs. Write answer.json with keys format_only (boolean), format_changed_ids (list), substantive_changed_ids (list), retention_days_before and retention_days_after (integers).",
     {"format_only": True, "format_changed_ids": [], "substantive_changed_ids": ["R1"],
      "retention_days_before": 30, "retention_days_after": 90},
     ["documents", "requirements", "revision"]),
    ("full-dataset-outlier",
     "Analyze all rows of measurements.csv, not just a preview. Write answer.json with integer keys rows_examined, sum, maximum, and string key maximum_id. Include every data row, excluding the header.",
     {"rows_examined": 150, "sum": 11490, "maximum": 10000, "maximum_id": "R137"},
     ["data", "coverage", "outlier"]),
    ("mixed-time-units",
     "Normalize durations.csv to milliseconds. Write answer.json with integer total_ms and fastest_id/slowest_id strings. Compare normalized durations, not raw numeric values.",
     {"total_ms": 4500, "fastest_id": "C", "slowest_id": "D"},
     ["data", "units", "time"]),
    ("exact-decimal-money",
     "Sum all amounts in money.csv exactly without binary floating point. Write answer.json with currency and total strings, preserving exactly two decimal places; include rows_examined as an integer.",
     {"currency": "USD", "total": "9007199254740993.30", "rows_examined": 4},
     ["data", "decimal", "money"]),
    ("contradictory-quotes",
     "Using source-a.txt, source-b.txt and source-register.json, decide whether the supplied sources establish launch approval. Write answer.json with status (resolved or unresolved), approved (boolean or null), and quotes: a list in source filename order of objects with source and exact quote. Preserve both opposing launch sentences verbatim. Do not choose an authority absent from the register.",
     {"status": "unresolved", "approved": None, "quotes": [
         {"source": "source-a.txt", "quote": "The launch is approved for March 4."},
         {"source": "source-b.txt", "quote": "The launch is not approved for March 4."}]},
     ["evidence", "ambiguity", "contradiction"]),
    ("nested-json-jsonl",
     "Analyze nested.json and events.jsonl. Sum quantity for SKU X across every nested batch/order/line. Normalize non-null event durations to milliseconds; null means missing, not zero. Write answer.json with integer keys x_quantity, orders_examined, events_examined, events_with_duration, total_duration_ms and a missing_duration_ids list.",
     {"x_quantity": 5, "orders_examined": 2, "events_examined": 3,
      "events_with_duration": 2, "total_duration_ms": 2250, "missing_duration_ids": ["E3"]},
     ["data", "nested_json", "jsonl", "coverage"]),
    ("spreadsheet-cache-boundary",
     "Inspect formula-cache.xlsx Totals!A3 without claiming a spreadsheet-engine recalculation. Write answer.json with cell (Sheet!coordinate), formula (without leading =), cached_value as a string, cache_verified as a boolean, and independently_computed_value as a string for the simple arithmetic expression. A cache alone does not prove verification.",
     {"cell": "Totals!A3", "formula": "A1+A2", "cached_value": "999",
      "cache_verified": False, "independently_computed_value": "3"},
     ["spreadsheet", "formula_cache", "uncertainty"]),
    ("ambiguous-transcription-double",
     "Read transcript-double.json. It is a synthetic transcription double, not a recording. Do not invent a resolved day or claim live transcription. Write answer.json with kind (copied from input), live_audio_processed (boolean), resolved_day (string or null), alternatives (list), segment_id, and start_seconds/end_seconds (numbers) preserving the ambiguous segment interval.",
     {"kind": "synthetic_transcription_double", "live_audio_processed": False,
      "resolved_day": None, "alternatives": ["Tuesday", "Thursday"],
      "segment_id": "S1", "start_seconds": 1.0, "end_seconds": 2.5},
     ["audio_double", "ambiguity", "uncertainty"]),
    ("scanned-document-boundary",
     "Read scan-boundary.json. This is only a synthetic boundary manifest; no scanned document or OCR result is supplied. Does empty extracted text establish an empty page? Write answer.json with page (integer), page_is_empty (boolean or null), ocr_status (copied), scan_processed (boolean) and extracted_claims (list). Do not invent OCR content.",
     {"page": 1, "page_is_empty": None, "ocr_status": "not_run",
      "scan_processed": False, "extracted_claims": []},
     ["scan_boundary", "coverage", "uncertainty"]),
]


def answer_command(expected):
    """Native command check: JSON object order is irrelevant; types and key coverage matter."""
    import shlex
    code = ("import json; from pathlib import Path; "
            "actual=json.loads(Path('answer.json').read_text()); "
            f"expected=json.loads({json.dumps(expected)!r}); "
            "assert json.dumps(actual,sort_keys=True)==json.dumps(expected,sort_keys=True), "
            "'answer does not meet value/type/coverage contract'")
    return "python3 -c " + shlex.quote(code)


def suite_text():
    lines = ['version = 1', 'name = "interpretation-offline-v1"', '',
             '# Synthetic evaluation: acceptance contracts, not a measured model-quality claim.']
    for case_id, prompt, expected, categories in CASES:
        lines.extend(['', '[[cases]]', f'id = {json.dumps(case_id)}',
                      'project = "examples/benchmarks/interpretation-fixture"',
                      f'prompt = {json.dumps(prompt + " Do not modify input fixtures.")}',
                      f'categories = {json.dumps(categories)}',
                      'checks = [{kind="command",command=' + json.dumps(answer_command(expected)) + '}]'])
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    from pathlib import Path
    Path(__file__).with_name("interpretation.toml").write_text(suite_text())
