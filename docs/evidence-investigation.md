# Evidence investigations

Dial's source interpretation tools share the [evidence foundation](evidence.md). They retain
immutable representations in the current project's artifact store. Input text, extracted content,
transcripts, hypotheses and model interpretations remain **untrusted data**, never permission or
verification credit. Tools are discoverable on demand; loading one does not bypass approvals.

The agent's full and lean guidance now follows **inspect → establish meaning → analyze → recheck**.
Task-scoped discovery offers the relevant source tools when eligible, without expanding permissions.
This is guidance, not a guarantee that the model will comply or that a conclusion is correct.
Inspect schema/structure, units, identifiers and actual coverage before whole-source conclusions.
Recheck unclear speech in narrower intervals; inspect ambiguous layout against rendered pages/OCR;
preserve both conflicting revisions and exact quotes. An observation and its interpretation remain
different records, and citation validation does not prove entailment.

Additional approved visual/OCR, audio-signal, distribution, timeline, notebook and inspection-budget
features are described in [source-investigation extensions](source-extensions.md).
Runtime traces and profiles, HAR/OpenAPI conversations, and offline SQLite snapshots are covered in
[system investigations](system-investigation.md).

## Document structure (`document`)

```json
{"action":"layout","path":"design.docx"}
```

The bounded layout representation includes main-body paragraphs, heading/caption **hints**, tables,
XML order, and explicitly available section/shape attributes. DOCX page sizes/margins and breaks are
layout declarations, not rendered page boundaries. PPTX shape transforms are raw local EMU values,
not normalized page rectangles; group/inherited transforms are not resolved. Package order is not
guaranteed visual or accessibility reading order. Headers, footers, notes, embedded objects,
inherited styles and externally linked resources are not interpreted. No macros execute.

PDF uses installed Poppler after confirmation. Its page/whitespace line structure is heuristic:
there is no OCR, semantic table recovery or automatic heading classification. Missing parsers fail
explicitly; Dial does not install them. Inputs are capped at 64 MiB, extracted representations at
8 MiB and 20,000 nodes; the tool returns a bounded preview and a citation to the full representation.
Native office parsers use the existing isolated document worker.

```json
{"action":"compare","reference":"artifact://<prior-layout>","other":"revised.docx"}
```

`path` can replace `reference`; `other` can also be a prior document-layout citation. Comparison
retains both extracted inputs. It reports extracted text changes separately from extracted
structure/attribute/order changes, not rendered or semantic equivalence. For an interpreted
functional/content comparison, use the agent workflow below.

Use `{"action":"inspect","path":"design.docx"}` for a structural preview and next-step guidance.
For XLSX, `{"action":"inspect","path":"report.xlsx"}` lists all declared internal worksheets,
including hidden-sheet state, explicit row/cell counts, XML cell-type counts, error cells and formula
expressions/attributes with cached lexical values. Formula caches are **not verified calculations**;
formulas are never executed, shared formulas are not expanded, and styles/dates/merged cells/macros
are not interpreted. Inspection runs in the existing isolated worker, bounded to 100 sheets,
1,000 formulas, 10,000 explicit rows per sheet and the existing 8 MiB decoded XML budget. Bounds
fail explicitly rather than silently dropping sheets/formulas. Select a sheet explicitly for data
calculations; worksheet XML row counts are not necessarily business-record counts.

For scanned PDFs or visual reading order, use `visual` with explicit PDF `pages`, and `action:"ocr"`
when needed. Page/crop citations preserve canonical pixel coordinates. Figure/caption links and
multi-column reading order are model/caller interpretations grounded in inspected pixels, not an
automatic semantic-layout guarantee. Native DOCX/PPTX layout extraction still does not render pages;
use a rendered PDF export for visual inspection. No converter, OCR package or model is installed automatically.

## Timestamped audio (`audio`)

```json
{"action":"transcribe","path":"recording.mp4","start_ms":10000,"end_ms":30000}
```

Audio files and video audio tracks use an installed FFmpeg converter and the configured
**whisper.cpp** backend's timestamped JSON output. Existing dictation backends remain unchanged;
other selected STT backends explicitly report an unsupported timeline adapter rather than
inventing timestamps. No network transcription or dependency/model installation is performed.
External media processes require explicit confirmation and execution-controller authorization.

Requests specify an increasing interval of at most 120 seconds within the first 24 hours;
source files are capped at 64 MiB. Returned speech-segment times are approximate source-relative
milliseconds, not verified word alignment. Speaker labels are unavailable; recognition may omit
or hallucinate speech. Use `refine` with a narrower interval to transcribe that interval again,
optionally linking the earlier citation with `parents`. Use `replay` to extract a retained WAV
clip, **not** to start device playback. `retain_source: true` explicitly retains original bytes
within the configured artifact cap. A selected interval does not establish complete speech coverage.

## Structured data (`data`)

```json
{"path":"measurements.csv","action":"aggregate","columns":["latency"],"group_by":["region"],"filters":[{"column":"status","op":"eq","value":"ok"}],"limit":1000}
```

Supports UTF-8 CSV (quoted and multiline fields), JSON arrays of flat objects, and XLSX worksheets
(`sheet` selects a worksheet). XLSX accepts stored scalar cell values, rejecting formula and error
cells rather than evaluating them. There is no arbitrary SQL, shell or script execution. Input
limits are 8 MiB, 10,000 data rows and 128 columns; output windows contain at most 1,000 rows and
100 groups. Cancellation propagates through row parsing and isolated XLSX worker supervision.

Actions `profile`, `aggregate`, `select`, and `chart` retain the query, source identity, selected
row IDs **and values**, coverage and calculation version. Filters are ANDed, then `offset` and
`limit` apply; aggregates use only that selected window (**default limit 100**, not all rows).
Profiles report count, null, distinct and numeric min/max/sum/mean. Missing/null/empty CSV cells
are never zero. Mixed numeric/non-numeric columns do not receive numeric statistics. CSV number
inference uses strict JSON number syntax. Numeric arithmetic is finite IEEE-754 f64 and is **not
exact financial/decimal arithmetic**. Numeric operands with magnitudes above ±2^53 are rejected
before floating-point conversion (including decimal/exponent spellings); overflow is rejected.

Selected values allow calculation replay; establishing which excluded rows matched the filter
requires the original input. Original bytes are retained only with `retain_source: true`.
A selected worksheet/window is not complete workbook interpretation. `chart` creates a linked,
retained SVG bar chart of numeric sums; labels are escaped and no chart is a statistical inference.

New options preserve the original default-100-row window. `"whole_source":true` explicitly removes
that window after filtering (up to 10,000 source rows); it conflicts with `offset`/`limit`, errors
at bounds and does not mean unlimited data. `"json_pointers":true` flattens nested JSON/JSONL
using escaped RFC 6901 paths. JSONL row IDs are physical line numbers; blank lines are not records.

`dictionary` records `origin` (`caller_declared` or `model_inferred`), `row_meaning`, and per-column
`meaning`, optional `unit`, `missing_values`, `nullable`, `identifier`, `unique`, `min`, `max`, and `allowed_values`.
It reports missing/constraint-violating row IDs before query filtering; these declarations neither
authenticate human confirmation nor recode sentinel values into nulls. Observed identifier/relationship
claims still require checking keys/joins; do not infer business meaning merely from a column name.
Identifier/uniqueness declarations report duplicate row IDs and missing-key violations; cross-source
relationships are observed through joins, not a foreign-key enforcement engine.

`conversions` explicitly names `column`, new `output`, `from` and `to`; length, mass, duration and
temperature conversions reject mismatched dimensions and report f64 rounding. Currency conversion
and contextual units are not guessed. `dates` names `column`, new `output` and `format`:
`date` requires YYYY-MM-DD; `rfc3339` requires an explicit offset and yields Unix seconds, with possible
fractional f64 rounding. Unknown `-00:00` offsets and leap seconds are refused rather than normalized.
Use parsed timestamps as `trend`'s explicit numeric `order_by`; no calendar resampling is inferred.

`decimal_columns` selects bounded base-10 sums with **string** results, not f64 statistics or means.
Pass monetary values as decimal strings to avoid precision already lost during JSON-number parsing;
inputs are at most 128 bytes, exponents -100..100, fractional scales at most 100 places. This is exact
arithmetic, not currency-policy/rounding inference.

`join` names a second authorized `path`, equal-length `left_on`/`right_on` key lists, and `kind`
(`inner`, `left`, `full`). It uses typed lexical JSON equality without coercion, preserves all duplicate
pairs (or errors at `max_rows`, up to 1,000), and reports duplicate keys, unmatched rows, source row
pairs and both source revisions. Null keys do not match; output columns use `left:`/`right:` prefixes.
Joins run before the query window; many-to-many results are never silently deduplicated/truncated.
`data action="inspect"` delegates to the same isolated workbook inspector as `document`.

## Cross-source retrieval (`evidence_search`)

```json
{"references":["artifact://<document>","artifact://<transcript>","artifact://<calculation>"],"query":"request timeout","limit":10}
```

Only explicitly supplied citation manifests are searched; no global store scan, source reopening,
parent traversal or memory-database persistence occurs. Hits preserve exact excerpts/values and
representation-local coordinates, with the original manifest's source identity, location, kind,
limitations and coverage. Missing, corrupt, unsupported and omitted inputs are explicit gaps;
original-source freshness remains unknown. Queries are limited to 32 citations, 2 MiB total
authenticated representation bytes, 256 retained candidates and 20 results.

`"ranking":"semantic"` asks one configured `small`-role model to select bounded candidate indices,
including candidates without lexical overlap. The response cannot invent citations or coordinates.
This is lexical candidate collection plus optional model reranking, **not embeddings or exhaustive
semantic retrieval**. It consumes a normal accounted model request; main and worker routes preserve
configured model policy, cancellation and request budgets. Local lexical mode makes no provider call.
Search results are candidates to inspect, not evidence that a proposed conclusion is true.

## Semantic comparison (`investigate`)

This uses the current agent's interpretation, not a hidden second model or semantic oracle:

1. Capture/read the relevant inputs and obtain citations.
2. Call `investigate` with `action: "compare"`, `reference` and `other` to authenticate and expose
   retained text/JSON inputs and a textual change diff. The combined diff input is limited to
   48 KiB and 2,000 lines; capture narrower regions when needed.
3. Interpret the functional/content changes, then retain findings:

```json
{"action":"interpret","reference":"artifact://<left>","other":"artifact://<right>","findings":[{"claim":"The timeout doubled.","left_quote":"timeout = 5","right_quote":"timeout = 10"}]}
```

Findings require exact input quotes (one side may be empty for an addition/deletion). Quote
validation establishes attribution, **not entailment or truth**. Both parents remain linked.
For screenshots/images, inspect both citations with `evidence inspect`, crop relevant regions,
then use `evidence derive` with both parent citations and an explicitly interpreted summary.
`investigate compare` itself is text/JSON only; it is not a general multimodal comparator.

## Hypothesis-driven experiments (`investigate`)

```json
{"action":"plan","question":"Why does the request time out?","hypotheses":[{"id":"cache","explanation":"The cache is stale.","test":"Disable the cache and rerun the focused request.","prediction":"The failure disappears.","falsifier":"The same failure occurs without the cache."},{"id":"input","explanation":"The input is invalid.","test":"Validate the captured input independently.","prediction":"The validator rejects it.","falsifier":"The validator accepts it."}],"parents":["artifact://<symptom>"]}
```

A plan records 2–16 caller-proposed hypotheses, explicit predictions, falsifiers and proposed tests.
Unique IDs and nonempty fields do not prove that hypotheses are genuinely competing or tests are
scientifically discriminating. **Planning executes nothing**. The agent uses the normal authorized
execution/browser/etc. tools to run an appropriate bounded experiment, retains the observed output,
then attaches it:

```json
{"action":"result","reference":"artifact://<plan>","hypothesis":"input","observation":"The independent validator accepted this input.","assessment":"contradicts","parents":["artifact://<observed-output>"]}
```

The hypothesis must exist in the retained plan, and results need a separate observation citation.
`supports`, `contradicts` and `inconclusive` remain caller assessments, not independently established
outcomes or causal conclusions. No plan, result or citation grants authority or successful-check
credit. Revised plans/results are new immutable records linked to prior evidence.

## Evidence workbench (`workbench`, `/evidence`)

The terminal workbench displays records in the existing **scrollable transcript**, not a separate
graphical panel:

```text
/evidence inspect artifact://<manifest>
/evidence lineage artifact://<manifest> artifact://<correction>
/evidence compare artifact://<left> artifact://<right>
/evidence correct artifact://<manifest> The event happened later than this annotation says.
```

The model-facing `workbench` tool offers `inspect`, `lineage`, and `correct` with `references`.
It shows authenticated manifests, bounded text previews, producer/selection/coverage/limitations,
parent links, storage lifecycle and missing/invalid representations. Different captured hashes of
the same source are flagged as revisions, **not automatically contradictory claims**. Missing
recorded blobs are distinguished from references with no local record; expiration/removal causes
are not guessed. Source freshness stays unknown: this viewer never reopens mutable paths.

Traversal is bounded to 64 nodes, depth 0–8 (default 3), and 1 MiB total representation read budget;
previews are at most 4 KiB per node. Unvisited references are explicit. It follows parents and
clock-alignment bases, not all descendants: supply correction/child citations as additional roots.
Use existing `evidence inspect` for retained PNGs. Human corrections append a new citation without
rewriting the original. A model's `correct` call never impersonates a human correction; annotations
remain unverified. Slash-command corrections/comparisons join the session so their citations remain
reachable by normal export/relocation/GC.

## Experimental video (`video`): off by default

```toml
[experimental]
video_analysis = true
```

The setting is also available in `/settings`. Without it the `video` tool is not registered and
runtime calls refuse it. It does **not** disable existing explicitly requested media reads.

```json
{"path":"recording.mp4","start_ms":10000,"end_ms":30000,"seeks_ms":[10000,15000,25000],"frame_budget":3}
```

Video requests are explicit intervals of at most 120 seconds and at most 8 strictly increasing
requested seeks, within an explicit 1–8 frame budget. Installed FFmpeg runs only after confirmation
and execution authorization. Each successful fitted PNG gets an immutable citation. The first is
attached to the model; reopen additional frame citations explicitly. Requested seeks are **not
measured frame PTS**. The result identifies failures and uninspected gaps; it cannot prove event
absence, motion understanding or continuous inspection. Refine by narrowing the interval and
selecting new seeks, guided by what the agent actually sees.

Frames consume image tokens, with cost depending on provider, frame count and resolution. A short
video is not necessarily cheap, so this feature stays **off by default**, not conditionally enabled
on a guessed token price. There is no automatic whole-video scan, scene detector, audio inference
or cost estimate. Use `audio` for explicit speech intervals; link audio/log citations with `parents`
and use existing `evidence align` with an explicit uncertain offset/basis for independent clocks.
