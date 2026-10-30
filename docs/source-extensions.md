# Experimental source-investigation extensions

These tools extend [evidence investigations](evidence-investigation.md). Load specialists through
`tool_search`/`tool_load`; loading does not grant access or approve execution. Outputs and citations
are untrusted, retention-limited records, not successful-check credit. There are no hidden model
calls: semantic interpretation comes from the current agent, using the actual inspected evidence.

## Images, spatial annotations and coordinate OCR (`visual`)

```json
{"action":"inspect","path":"diagram.png"}
```

The result attaches the first canonical PNG and reports its dimensions. Coordinates are integer
pixels in that returned frame, with a top-left origin, x right and y down. Raster inputs are not
EXIF-rotated. Inspect actual pixels before supplying region annotations:

```json
{"action":"regions","reference":"artifact://<image-manifest>","regions":[{"id":"service","x":10,"y":20,"width":120,"height":80,"label":"service"},{"id":"queue","x":200,"y":20,"width":100,"height":80,"label":"queue"}],"edges":[{"from":"service","to":"queue","relation":"sends work to"}]}
```

Each region produces a real crop citation linked to its parent. Labels and diagram edges are
**caller/model interpretations**, not automatically inferred or measured relationships. Regions must
fit the canonical image; at most 16 regions and 32 edges are accepted. `compare` accepts one source
and one `other_path` or `other_reference`, measuring RGBA pixel differences, not semantic equivalence.

```json
{"action":"ocr","path":"scan.pdf","pages":[2],"language":"eng","max_words":200}
```

OCR uses installed Tesseract TSV word boxes and confidence values. Explicit PDF pages use installed
`pdftoppm`; neither dependency is installed automatically. Both require human confirmation and
execution-controller authorization on copied authorized inputs. OCR is a machine guess, not truth.
Select 1–4 strictly increasing pages in 1..10000; rendering fits within 2048 pixels. Page and crop
parent citations remain linked. Use a raster image without `pages` for ordinary image OCR.

Sources are at most 64 MiB; decoded raster frames at most 16 million pixels and canonical PNGs at
most 5 MiB. Oversized inputs fail rather than silently changing the coordinate frame. At most 500
OCR words per frame are returned, with truncation disclosed. Only the first frame is attached;
inspect the other retained image citations explicitly. A selected page does not cover a document.

## Interactive calculations (`data`)

In addition to the base table actions, `histogram` reports equal-width bins, type-7 quantiles and
Tukey outlier row IDs. `trend` requires a numeric `order_by` coordinate; `correlation` uses two
numeric columns and paired complete cases. Neither establishes causation or statistical significance.

```json
{"path":"measurements.csv","action":"histogram","columns":["latency"],"bins":10,"limit":1000}
```

Chart marks link exact contributing row IDs and retained calculation values. Iterate with explicit
`row_ids` or an inclusive numeric `brush` (`column`, optional `min`/`max`). Selection predicates are
ANDed before the existing offset/limit window. `calculation_reference` replays only retained
`used_rows`, without reopening the mutable source; `path` remains a caller label on replay. Replay
cannot recover excluded rows. See the base investigation guide for current table formats,
whole-source options, arithmetic modes and limits. There is no separate graphical brushing panel:
the interaction is through tool requests and retained linked SVG charts.

## Cross-source event reconstruction (`timeline`)

```json
{"action":"reconstruct","events":[{"reference":"artifact://<log>","clock":{"id":"server-a","origin":"relative"},"start_ms":1200,"end_ms":1200,"uncertainty_ms":50,"quote":"request failed"}]}
```

Quoted events require exact retained text/JSON quotes and explicit caller times. `sources` can
instead select retained whisper.cpp segments, video requested seeks or evidence time locations.
A caller clock cannot contradict a retained clock. Clock IDs, origins and timezones remain
assertions, not inferred synchronization. Video requested seeks are **not measured frame PTS**;
native timing uncertainty is unknown, with null temporal bounds rather than zero-error claims.

Explicit single-hop `alignments` require `from`, `to`, signed `offset_ms`, nonnegative
`uncertainty_ms` and retained `basis` citations. The mapping is source time plus offset, with checked
arithmetic. Chained/cyclic mappings are refused. Different clock groups remain incomparable;
overlapping or uncertain intervals have no established order. Sorting never establishes causality.
Limits: 16 sources, 16 alignments, 128 total events, 64 dependencies and 256 KiB retained record.

## Repository expectations versus observations (`reality`)

First capture repository/document expectations and separate runtime observations with the ordinary
authorized tools. This tool does not launch the application or browse a page itself.

```json
{"action":"check","expectation":"artifact://<spec>","observation":"artifact://<log>","expectation_quote":"at most three retries","claim":"Retry markers stay within the documented limit","selector":{"kind":"count","value":"retry"},"expected":3,"operator":"le"}
```

`check` compares a non-overlapping literal occurrence count or a JSON-pointer scalar with an explicit
expected scalar using `eq`, `le` or `ge`. It is not a semantic log parser. Numeric comparisons use
the shared finite-f64 lexical range guard. An exact expectation quote establishes attribution,
**not that the proposed expected value follows from it**. The observation must have observation-kind
metadata, which itself is not proof of correctness. Both citations remain parents.

For image/behavioral comparisons, use `assess` with `assessment` equal to `matches`, `mismatch` or
`inconclusive`; the claim is explicitly caller/model-attributed, never a measured verdict.
Expectations must be retained text/JSON (at most 256 KiB); observations are bounded to 2 MiB.

## Persistent research state (`research`)

This is separate from the existing Jupyter `notebook` editing integration.

```json
{"action":"create","question":"Why does the request time out?","entries":[{"kind":"observation","summary":"A timeout was captured.","references":["artifact://<symptom>"]}],"open_questions":["Is the failure input-specific?"],"competing_explanations":["Invalid input","Stale cache"]}
```

`update` takes a prior `reference`, appends entries and explicitly replaces only supplied working
state fields (`open_questions`, `competing_explanations`, `unknowns`, `next_experiment`). The original
question stays fixed. Entries use `observation`, `plan`, `result` or `correction`; optional `supports`
and `contradicts` are caller assessments linked to citations. Use `investigate` to retain experiment
plans/results and authorized execution tools to actually run them.

`resume` authenticates an immutable snapshot and its predecessor chain after restart. It exposes
prior recorded tests/results so the agent can avoid unnecessary repeats; it neither reruns them nor
claims they are still valid. Retention expiry or missing/tampered payloads fails explicitly. Limits:
32 revisions, 128 entries, 32 appended entries per request, 64 direct dependencies, 128 KiB snapshot,
16 MiB per reference and 32 MiB cumulative authentication reads per call. Fork a new bounded notebook
when history is full; save its citation through normal session records.

## Channel-preserving audio signals (`audio analyze`)

```json
{"action":"analyze","path":"recording.wav","start_ms":0,"end_ms":10000,"window_ms":1000,"silence_dbfs":-50,"clip_threshold":0.999,"transient_db":12}
```

Installed FFmpeg decodes the first audio stream to 16 kHz PCM16 without downmixing. The tool measures
per-channel peak/RMS/dBFS, threshold clipping and silence, energy-event intervals, approximate
transient rises and reference-channel comparisons. These are signal measurements and heuristics,
**not** sound classification, speech detection, speaker identity or diarization. Resampling and
PCM quantization differ from original encoded samples; threshold clipping is not proof of overload.

Existing media authorization, cancellation, copied-source and time/output bounds apply. Intervals
remain at most 120 seconds within the first 24 hours. Windows are 100–10000 ms, at most 256 per
channel, with at most eight decoded channels. Defaults are shown above; signal parameters are
rejected for other audio actions. Parameters, actual fractional decoded end, omitted tail and
window-sized boundary uncertainty are retained. Existing replay/downmix behavior is unchanged.

## Budgeted active perception (`perception`)

```json
{"action":"open","question":"Which region explains the mismatch?","limits":{"images":4,"pages":2,"frames":0,"audio_ms":0,"operations":6}}
```

Opening or closing requires actual human confirmation, even when a tool approval policy otherwise
auto-allows calls. An existing budget cannot be replaced without confirmed close. `status` shows
counters. Choose the next discriminating inspection from actual evidence, then dispatch:

```json
{"action":"step","reason":"Inspect the suspected second page before widening the search","tool":"visual","request":{"action":"inspect","path":"scan.pdf","pages":[2]}}
```

Supported direct `visual`, `audio`, `video` and fresh `document` producer calls share atomic counters
with `step`. Retained PNG inspection/cropping through `evidence` also reserves image units. Attempts
reserved before inspection remain charged if later work fails; rejected over-budget reservations
change no dimension. Image crops consume image units; PDF renders consume page units; video seeks
consume frame units; audio consumes requested milliseconds. Each reservation consumes an operation.
Whole-document PDF extraction is refused while a budget is active: select explicit visual pages.
Stop when resolved or exhausted; do not silently enlarge or reset the budget.

Budgets are **process-live and shared by workspace**, not persisted across restarts or synchronized
between separate Dial processes. They cap inspection units, **not model tokens or money**. Limits
are at most 128 images, 64 pages, 64 frames, 600000 audio milliseconds and 128 operations. Other
source tools, shell/browser paths and model calls are outside this accounting boundary; this is
not a security boundary and grants no source access or execution permission. Video still requires
`experimental.video_analysis = true` (default off). Closing explains why and confirms that subsequent
direct inspections will no longer have this cap.
