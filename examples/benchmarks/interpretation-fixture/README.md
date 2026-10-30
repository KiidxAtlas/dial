# Synthetic interpretation evaluation inputs

`../interpretation.toml` is a version-1 Dial benchmark suite with nine independent
cases. Each case requests `answer.json` and grades its **values, JSON types, and
coverage**, not tool counts, phrasing, or a particular implementation. Object key
order and whitespace do not matter; list order is specified where meaningful.
The executable command checks and answer contracts live outside the copied
project, following the existing benchmark convention. A model can inspect these
public fixtures in advance: this is a regression set, **not a held-out benchmark**.

## Cases and boundaries

| Case | Adversarial condition | Expected distinction |
| --- | --- | --- |
| Requirements revisions | Same requirements with Markdown-only changes vs 30→90-day retention | Formatting is not a substantive requirement change |
| Full dataset | 150 rows; maximum at data row 137, beyond a 100-row preview | Full coverage totals 11,490, not a preview extrapolation |
| Time units | Milliseconds, seconds, minutes and hours reverse raw-value rankings | Normalize units before comparing or totaling |
| Money | Decimal fractions plus a value beyond binary64 integer precision | Exact USD total `9007199254740993.30` |
| Contradictory quotes | Same-date unratified drafts; no authority order | Preserve both quotes, leave approval unresolved |
| Nested JSON / JSONL | Nested batches/orders/lines; one null duration | Include nested records; missing is not observed zero |
| Formula cache | Real small XLSX ZIP, `A1+A2`, cached 999, inputs 1 and 2, manual calculation | Independent arithmetic 3 does not verify the stored cache |
| Audio ambiguity | Explicit synthetic transcription double, ambiguous weekday and interval | Preserve alternatives; do not claim live transcription |
| Scan boundary | Explicit synthetic manifest, empty extraction and OCR not run | Unknown content is not an empty page |

All inputs are synthetic and small. The spreadsheet is an actual Open XML package,
but no spreadsheet calculation engine is exercised by the offline checks. The
audio case supplies **no recording**; the scan case supplies **no scanned page**.
They evaluate uncertainty reporting from supplied metadata, not transcription,
image understanding, PDF rendering, or OCR accuracy. Do not describe them as
processed audio/scans. Quotes and intervals are fixture content, not observations
of real people or events.

## Repeatable offline checks

From the repository root, using Python 3.11+ and only the standard library:

```sh
python3 examples/benchmarks/generate_interpretation.py
python3 examples/benchmarks/interpretation_cases.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s examples/benchmarks -p test_interpretation.py -v
```

The two generation commands reproduce the committed inputs and TOML. ZIP entry
order, timestamps and permissions are fixed. Tests independently compute the
numeric answers from actual CSV/JSON/XML input bytes, check reproducibility, and
exercise every native command grader with missing, correct, wrong-field,
partial, and malformed answers. Oracle-written answers are **grader tests**, not
agent solutions. A passing offline run says nothing about live model quality or
native Dial tool execution.

For quality measurements, run the TOML through the existing Dial benchmark runner
with an explicitly configured provider, retain its ordinary report, and compare
repeated trials. That is an optional later tier needing model access; it is not
part of these offline tests. These cases use ordinary-language requests rather
than pinning evolving specialist tool parameter names. No registry changes or
optional OCR/transcription packages are needed.
