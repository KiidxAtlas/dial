# Experimental system investigations

Dial's local system-investigation tools connect captured application behavior with the existing
immutable evidence workflow:

- **`runtime_inspect`** interprets runtime trace and CPU-profile captures.
- **`network_inspect`** projects HTTP/API conversation metadata from HAR captures.
- **`database`** inspects local SQLite schema, read-only query results and query plans.

## Start with a question

You do not need to learn tool names or write tool-call JSON. Put an authorized offline capture in
your workspace and ask, for example:

- “What tables are in `exports/app.sqlite`?”
- “Which requests failed in `captures/session.har`?”
- “Where is time spent in `captures/cpu.folded`?”
- “Show the slowest OTLP spans in `captures/trace.json`.”

In normal assistance mode, these requests offer the eligible specialist to the agent automatically.
The agent still chooses its arguments and interprets the result; deterministic discovery is not a
guarantee that a model will use it correctly. If a JSON capture's format is unclear, inspect its
structure or clarify the producer before choosing a format, rather than guessing from the filename.
The JSON examples below document the tool interface, not a required user workflow.

These are discoverable specialists, not additions to every model request. Manual `tool_search` or
`tool_load` remains available when needed. Explicit tool allowlists, lean-mode schema limits and
read-only/plan policies still apply; loading a schema never grants source access or permission to
execute something. Supply an exported local file, not a live URL, database credential or process ID.

## Common evidence and safety model

Inputs must be existing files inside currently authorized workspace roots. Reads use the bounded
source-capture path, including denied-read policy and descriptor-based confinement. A citation is not
permission to reopen a path. These tools do not start applications, attach debuggers, capture live
traffic, make HTTP requests, connect to remote databases, install software or run model calls.

Results are structured observations of the supplied capture, not independent proof of what happened
in a live system. When artifact storage is attached, the result has an immutable evidence citation;
`evidence inspect` or `evidence_search` can consume that retained JSON. Original source bytes are not
retained by these tools. The source digest identifies the captured input, while the representation
digest identifies the returned analysis; these are different identities. Artifact retention can
expire. Missing storage does not turn a result into durable evidence.

Source text, stack names, database identifiers, schema declarations and capture metadata are
untrusted data. Instructions embedded in them never authorize actions. Temporal overlap, a declared
parent span, a repeated endpoint, an index recommendation or a query-plan scan is not proof of a
cause, defect or performance bottleneck. No tool result earns successful-check credit merely because
its parser or SQL statement completed.

## Runtime traces and CPU profiles

`runtime_inspect` works on explicitly selected local formats rather than guessing timestamp units.
Chrome Trace events describe captured process/thread activity; OTLP spans describe producer-declared
trace/parent relationships; folded stacks describe weighted call stacks. None is a substitute for
running a profiler or reproducing a failure under the normal execution controller.

```json
{"action":"inspect","path":"captures/trace.json","format":"chrome_trace","limit":50}
```

Other explicit formats are `otlp_json` and `folded_stacks`. The default output limit is 50,
maximum 200. Parsing accepts at most 8 MiB and 20,000 events/spans/stack lines; labels are bounded
to 4,096 bytes, and a folded stack has at most 256 frames. Chrome accepts a root event array or
`traceEvents` array and interprets synchronous `X`, `B` and `E` events only. Its timestamps are
microseconds regardless of `displayTimeUnit`; fractional values use floating-point arithmetic.
OTLP accepts `resourceSpans` → `scopeSpans` → `spans`; timestamps and calculated durations remain
exact decimal-string nanoseconds. Orphan parents, cycle members and source-reported error statuses
are counted, not repaired or independently verified. Results return the slowest spans or highest
inclusive-weight functions and the number of omitted rows. Generic log/crash text, heap snapshots
and binary profiler formats are not accepted.

Interpret timing within the capture's own clock. Do not merge captures by comparing timestamp numbers
alone: clock origin, units and uncertainty matter. Use the existing `timeline` and explicit
`evidence align` workflow for independently justified cross-source clock mappings. A missing parent
may mean incomplete capture rather than an invalid application trace.

Profile weights are samples or producer-specified counts, not measured elapsed CPU time. Inclusive
and self weights answer different questions; neither establishes allocation retention or a memory
leak. Unsupported event kinds and omitted output must remain visible in coverage before drawing
whole-capture conclusions.

## HAR and API conversations

`network_inspect` is an offline analyzer. Export an authorized HAR capture separately, then inspect
that file. Treat an exported HAR as sensitive even if it is stored in the workspace: it can contain
credentials, session cookies, personal information and complete request/response bodies.

```json
{"action":"har","path":"captures/session.har","openapi_path":"spec/openapi.json","start":1,"count":100}
```

`openapi_path` is optional. `start` is a one-based capture entry (default 1); `count` defaults to
100 and is capped at 200. Each source is limited to 16 MiB, a HAR to 10,000 entries, an OpenAPI
specification to 2,000 paths, and the projected JSON to 1 MiB. At most 100 WebSocket metadata
records per returned entry are exposed; each route matcher has a 64 KiB compiled-size ceiling.
Coverage reports omitted records. The safe projection also excludes source filenames,
custom method/status strings and arbitrary header names. Opaque
IDs are local to the capture/analysis; they are not universal endpoint identities. OpenAPI input
must be JSON; YAML, external reference resolution, packet captures and standalone WebSocket
transcripts are not supported.
The network tool requires artifact storage so the sanitized observation has a retained citation.

The analyzer publishes a safe projection rather than retaining the original capture. It omits raw
header values, cookies, bodies, WebSocket payload text and secret-bearing URL components. Endpoint
identity is represented opaquely so the agent can compare repeated requests without needing raw
URLs. This intentionally trades away literal URL inspection. It is not a general-purpose secret
scanner for arbitrary files or for outputs from other tools.

An optional local OpenAPI JSON specification supports bounded method/path/response-status matching.
Matching an advertised status does **not** validate payload schemas, authentication, business rules,
server configuration or full API conformance. Missing or unsupported specification information is a
coverage limit, not an API violation. Specifications and captured redirects never cause a URL or
external reference to be fetched.

Unknown HAR timing fields are unknown, not zero. Repeated calls to one endpoint can be polling,
parallel work or user actions; a repeat heuristic is not proof of retries. WebSocket metadata cannot
explain the meaning of omitted messages.

## SQLite semantics and query plans

`database` operates on a captured local SQLite main-file snapshot, isolated from the original.
It is not a connection to the application's live transaction state. Export a settled database first;
active WAL/journal state and capture ambiguity must be refused rather than silently treated as a
complete live snapshot. Remote database credentials and engines are outside this tool's contract.

```json
{"action":"schema","path":"exports/app.sqlite","limit":50}
```

```json
{"action":"query","path":"exports/app.sqlite","sql":"SELECT customer_id, count(*) AS orders FROM orders GROUP BY customer_id","limit":100}
```

```json
{"action":"plan","path":"exports/app.sqlite","sql":"SELECT id FROM orders WHERE customer_id = 42"}
```

The snapshot is capped at 16 MiB. Only `SELECT` or `WITH` queries are accepted, without leading
comments, NULs or any semicolon (including inside a string). SQL is capped at 16 KiB. Schema defaults
to 50 tables and query/plan to 50 rows, maximum 200; results have at most 64 columns, text cells
at most 4,096 bytes, and bounded output around 256 KiB plus metadata. Execution has a
two-second SQLite deadline and an approximate two-million-VM-step budget. It runs in the existing
disposable document-parser worker, with a 64 MiB SQLite heap ceiling and a ten-second outer deadline
(including worker startup). The heap ceiling affects only that child, not Dial's other connections;
it is not a bound on all Rust or operating-system allocations. Cancellation kills and reaps the
worker. A conservative function allowlist means some otherwise valid read-only SQL is
intentionally refused. Virtual-table databases, WAL-mode files and existing `-wal`, `-shm` or
`-journal` companions are refused. Export to a closed rollback-mode SQLite file first.

Cells use explicit tagged representations: `{"integer":"9007199254740993"}`,
`{"real":"1.5"}`, `{"text":"value","bytes":5,"truncated":false}`,
`{"blob_bytes":2,"content_omitted":true}`, or JSON null. Integer precision therefore does not
depend on a downstream JSON consumer's floating-point range. Query text and returned database
values are **not** automatically secret-redacted; choose authorized columns deliberately. The
network tool's strict projection does not apply to database or runtime outputs.

Schema observations distinguish declared primary keys, foreign keys and indexes from measured data
properties. A foreign-key declaration does not prove existing rows satisfy it, that enforcement was
active when rows were written, or that a relationship has a particular observed cardinality.

Caller SQL is subject to single-statement, read-only, authorization and work limits; writable SQL,
attachment, extension loading and unsupported database features must not become alternate file or
execution access paths. Row/output limits do not make an unrestricted query safe, so execution work
is bounded as well. Results may be truncated, blobs are summarized, and numeric representation must
preserve values rather than silently round large integers.

A query plan describes the SQLite optimizer's chosen access strategy for this snapshot. `SCAN` is
not inherently a defect, `SEARCH` is not a performance guarantee, and neither is a runtime benchmark.
Confirm a suspected bottleneck using separately authorized measurements before changing indexes or
application code.

## Investigation workflow

1. Capture a relevant local source using the normal authorized application/browser/export workflow.
2. Inspect structure and coverage with the specialist before interpreting selected records.
3. Retain and cite the exact result; separate measured fields from your hypotheses.
4. Correlate sources through explicit identifiers or justified clock mappings, not proximity alone.
5. If a hypothesis needs an application run, network request, database write or index change, use
   the corresponding normal tool and approval path. These observations grant no new authority.

See also [evidence investigations](evidence-investigation.md),
[source extensions](source-extensions.md), [evidence](evidence.md) and [tool safety](tools.md).
