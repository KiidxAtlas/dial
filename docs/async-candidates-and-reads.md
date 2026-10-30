# Opt-in candidate and read overlap

These paths do not turn themselves on or update saved settings. `solve.enabled`,
`experimental.streaming_dispatch` and `experimental.tool_speculation` retain their
existing disabled defaults.

## Solve candidates

`solve.concurrency` defaults to **1**. Explicit values from 1 through 64 bound candidate
work, additionally capped by `task.max_concurrency`. With a local loopback endpoint,
candidate generation uses the existing worker queue and shared worker admission limits.
In-process local inference retains its serial generation lane. Evaluation can overlap
up to the configured bound; no model server or verifier is started automatically.

Every candidate starts from its own copy of the same dirty/untracked source snapshot.
Generation and assessment run in bounded waves; each wave's copies are deleted before
the next is created, retaining only the best passing tree across waves. Even unlimited
candidate counts do not retain all candidate snapshots at once.
Verifier commands still require approval, run in independent throwaway copies, and
cannot change the candidate selected for promotion. Candidate outputs are reduced in
request order: the smallest passing patch wins, with earlier candidates winning ties.
The controller waits for assessment, checks interruption and source freshness, and
uses the existing rollback-aware apply path. Parallelism never permits extra paths,
network access, remote model routing, or automatic worker merges.

## Native read preparation

`experimental.tool_speculation` optionally prepares native single-file `ast_search`,
`read` and `grep` results in the existing workspace preparation pool. Controller and
child execution use their ordinary tool admission and result-delivery paths. The pool
holds at most two preparations for five seconds and accepts source files up to 2 MiB.

Reuse requires the same owner, tool name and arguments, and the same actual source
bytes. Results are prepared with isolated edit witnesses; reading ahead does not grant
editing authority. Only successful delivery publishes the exact displayed row ranges,
with a fresh source check. Cancellation, changed source, missing/partial observations,
errors, denied admission, unsupported formats and expired preparations fall back to
ordinary execution. Plain (non-hashline) reads are not reused. Directories, URLs,
notebooks, document/image/archive formats and multi-path reads are excluded.

Preparation does not bypass approval, callback gates, pre-tool hooks, ordinary schema
validation, or current authorization. It does not add another worker pool for admitted
read-only batches: those continue to use the existing parallel-read dispatcher.

## Verification scope

The deterministic unit suites exercise isolated row-witness publication, stale and
cancelled result refusal, bounded source exclusions, candidate assessment overlap and
ordered failure handling. A live local model server and platform sandbox are separate
optional integration prerequisites; unit checks do not establish their availability.
