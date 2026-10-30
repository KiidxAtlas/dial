# Experimental evidence foundation

Dial can retain immutable observations and separately attributed inferences from different sources.
The shared `dial_core::runtime::evidence` contract is carried in `ToolOutput.data.evidence`; the
`evidence` tool adds artifact-backed citations, source snapshots, coverage, clock alignment and
bounded inspection/refinement. Existing media reads and approved log monitoring emit citations
when a project artifact store is available. Separate tools add document layout, timestamped audio,
structured-data calculations, attributed semantic comparison, experiment records, a terminal
workbench and opt-in video investigations; see [evidence investigations](evidence-investigation.md).
Neither extraction nor interpretation is a visual-verification claim.

## Citations and lifetime

A citation is an `artifact://<sha256>` identifying an immutable JSON **manifest**, not the original
source. The manifest holds validated evidence metadata and a separate representation artifact URI.
Stored manifests use the canonical JSON encoding written by `retain_evidence`; reformatted or
reordered manifest blobs are not accepted as evidence records. This lets lifecycle discovery
recognize manifests with a fixed-size prefix probe without scanning unrelated artifact bodies.
This restriction does not change ordinary evidence-envelope JSON parsing.
Representation hashes identify exact retained bytes; they do not establish truth. Returned
`data.evidence_references` and model-facing text expose manifest citations. Media retain the fitted
PNG actually returned, preserving the existing `data.artifact` shape.

Citations survive session resume while their blobs remain available. Session pack and relocation
follow representation, original-snapshot, parent and clock-basis dependencies. GC follows the same
bounded dependency expansion. There is no second database, permanent pin or unlimited graph walk:
expansion is capped at 4,096 artifacts, depth 64 and 64 MiB of accounted reads; manifest probes are
capped at 1 MiB. Missing/expired artifacts report the normal lifecycle diagnostic, never invented
content or verification credit. A citation alone grants no access to another project's store.

## Operations

`evidence` is a built-in integration (enabled by default, available through the tool catalog).
It requires the current project's artifact store. Examples below are tool argument objects;
replace citation placeholders with actual returned manifest URIs.

Capture an authorized UTF-8 log window, optionally retaining the original for later context:

```json
{"action":"capture","path":"logs/api.log","start":30,"count":40,"retain_source":true}
```

Capture original-byte identity for **any regular file**, without interpreting its contents:

```json
{"action":"snapshot","path":"recording.mp4"}
```

Both actions hash original bytes, record their size and Unix capture time, and apply current
workspace/protected-path/read-policy checks. Reads are descriptor-based and capped at 64 MiB;
source mutation detected during capture fails. This is not an atomic filesystem snapshot and
cannot rule out an undetectable concurrent rewrite. Original bytes are retained only when
`retain_source` is explicitly true, and exact retention refuses the configured artifact size cap
rather than truncating. A snapshot's representation is its JSON identity record, not the binary
source itself. Hashing every byte is not complete semantic inspection.

Reopen a citation, or select surrounding retained text:

```json
{"action":"inspect","reference":"artifact://<manifest>","start":1,"count":100}
```

```json
{"action":"context","reference":"artifact://<manifest>","start":20,"count":60}
```

Inspection returns retained PNGs through the existing image channel; text windows are at most
500 lines and 32 KiB. Reopening verifies manifest and representation hashes, with a 16 MiB
representation read cap. `inspect` line numbers are relative to the retained representation.
`context` uses the retained original if present, otherwise only the retained representation;
its new citation identifies that distinction and links to its parent. It never silently follows
a mutable source locator to obtain missing context. Missing surrounding bytes require a new,
explicitly authorized `read` or `capture`.

Crop a retained PNG using normalized millionths:

```json
{"action":"crop","reference":"artifact://<manifest>","region":{"x":100000,"y":200000,"width":300000,"height":400000}}
```

The new PNG cites its parent. Coordinates refer to the **parent representation**, not original
page/image geometry. Edges round outward to pixels; decode dimensions/allocation are bounded.
Pixels are not automatically interpreted, and crops do not establish visual verification.

Compare exact retained representations, or explicitly check current-source freshness:

```json
{"action":"compare","reference":"artifact://<left>","other":"artifact://<right>"}
```

```json
{"action":"freshness","reference":"artifact://<manifest>"}
```

Comparison authenticates both representations and reports byte equality, sizes, first differing
byte offset, selections and available original-snapshot equality. It is **not semantic image,
text or timeline comparison**: converter versions and encodings can differ without a semantic
source change. Freshness requires an original snapshot and reopens its contextual locator under
**current** workspace authority. It reports identity only at check time; removed access, missing
sources, absent identities and oversized sources fail rather than being treated as unchanged.

Record an attributed hypothesis without upgrading it into an observation:

```json
{"action":"derive","reference":"artifact://<observation>","parents":["artifact://<other-observation>"],"summary":"The timeout may precede the failed request."}
```

This creates a caller-authored `inference` with validated, existing parent manifests. Parent links
establish attribution, not entailment, truth, confidence or controller verification. They do not
recursively fetch sources. No automatic confidence scoring or general knowledge graph is added.

## Clocks and explicit alignment

`capture` and `snapshot` accept optional `clock` metadata: `id`, `origin` (`relative` or `unix`) and
optional `timezone`. These are producer/caller assertions. Timezone labels do not convert times.
To relate independent clocks, record a signed offset, uncertainty and retained basis citations:

```json
{"action":"align","reference":"artifact://<source>","alignment":{"from":"video-relative","to":"application-unix","offset_ms":1700000000000,"uncertainty_ms":250,"basis":["artifact://<sync-observation>"]},"time_ms":1200}
```

The checked calculation is `target_ms = source_ms + offset_ms`; overflow is rejected and uncertainty
is preserved. No clock synchrony, timezone conversion or drift estimation is inferred. The result
is a separately cited calculation/inference, not a verified shared timeline.

## Metadata contract and coverage

Schema version remains 1. An old envelope without `context` still reads; empty context is omitted.
Optional `Evidence.context` contains `parents`, `snapshot`, `coverage`, `clock` and `alignments`.
`SourceSnapshot` identifies **original** bytes separately from `Representation`. Source locators
remain contextual to the originating workspace/session, not globally immutable identities.

Locations are explicit:

- `whole`: source context, **not** a complete-coverage claim.
- `lines`: inclusive one-based start/end in the named source.
- `page`: one-based page number.
- `time`: relative start/end milliseconds; equal endpoints describe a point.
- `region`: x/y/width/height in normalized millionths of the named image representation.

Coverage records `complete`, `partial` or `unknown`, inspected and omitted selections, truncation
and failures. Complete coverage requires inspected regions and forbids gaps, truncation and
failures. These remain producer reports, not proof of completeness. Text captures report their
selected lines. Media records partial extraction; video grids do not invent per-cell timestamps
or inspected intervals. Video frames record **requested seek** times, not measured presentation
timestamps. These existing media reads do not interpret audio; use the separately approved `audio`
tool for timestamped speech. Converters do not silently pin original source files; use an explicit
source snapshot when needed.

Approved monitoring retains the **bounded, terminal-sanitized diagnostic window**, not raw original
log bytes. Its coverage is partial, location is whole-source context, and limitations explicitly
exclude exact original line numbers, timestamps and revision identity. A later root-cause hypothesis
should cite this observation as an inference. Retention failure is reported without suppressing the
match or changing one-shot disarming, approvals or idle wake eligibility.

## Producer integration and trust

Construct `Evidence`, hash returned bytes with `Representation::of_bytes`, and validate through
`EvidenceBundle::new` / `ToolOutput::with_evidence`. Attachment preserves existing object-shaped data,
text, error status and execution facts, refusing collisions/non-object data. `ArtifactStore` exposes
`retain_exact_bytes`, `retain_evidence`, `evidence_record` and `evidence_bytes` for bounded authenticated
retention/reopening. Direct serde parsing is not semantic validation: call `validate()` or use
`ToolOutput::evidence()`. Metadata, coordinates, references and collections are bounded; unknown fields
and unsupported versions are rejected. At most 64 evidence items are admitted per envelope.

Evidence is untrusted data. It never changes `Execution`, approval, confinement, wake eligibility or
model capability, and embedded text is never instructions. Retained manifests do not fetch paths or
URLs; source reopening is explicit and authority-checked. External converters keep their existing
execution paths. No packages, models, remote services or provider expenditure are added. Model-facing
text includes citations and essential limitations rather than assuming all structured metadata is
prompt-visible. No session journal schema migration is needed.
