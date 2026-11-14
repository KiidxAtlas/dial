# Tool audit: coverage, executable evidence and finding closure

The audit runner inventories configured tool **families**, assigns owners, executes reviewed
checks and rejects unsupported coverage/closure claims. It is not a promise to discover every bug.
The inventory comes from the owning Rust configuration declarations, including mode-only tools;
it does not enumerate tools dynamically supplied by MCP servers, plugins or debug adapters.
A configured integration is not evidence that its service is connected or working.

The checked-in plan is [`tool-audit.json`](../examples/benchmarks/tool-audit.json); the runner is
[`tool_audit.py`](../examples/benchmarks/tool_audit.py). This is developer tooling, not a new
agent tool or terminal command. See [benchmarks](benchmarks.md) for the existing measurement
contract and [requirement verification](requirement-verification.md) for turn-level checks.

## What is enforced

- Every discovered family must have exactly one owning group. Unknown declaration syntax,
  duplicate assignments, missing owners/sources and inventory drift fail validation.
- Every group declares all eight dimensions: `contract`, `effects`, `permissions`, `recovery`,
  `cancellation`, `concurrency`, `presentation` and `behavior`. Each has executable check IDs
  **or an explicit gap with a reason**. A gap is never a pass.
- Native contract tests cannot satisfy the behavior dimension. A live behavior check must name
  the group's tools and independently checked tasks; their execution must appear in the trace.
- A passing receipt needs a successful process, actual non-skipped test successes meeting the
  minimum, no test failures, no timeout/interruption and unchanged source inputs. An exit code
  alone, a zero-test filter, compilation success or an all-skipped suite is insufficient.
- Receipt consumers verify the command definition, log hash, recomputed test counts and current
  source fingerprint. Source includes untracked implementation/test files; changing source during
  or after a run invalidates it. Duplicate/conflicting receipts are refused rather than cherry-picked.
- Closed findings need an observed failing regression and a later fresh passing run of the same
  check, plus changed declared implementation/test inputs. Timeouts, missing runtimes and unittest
  import failures are not behavioral reproductions. Changing only the audit plan is insufficient.
- The Git baseline ratchet rejects removal of ownership, mapped checks or findings; reduced
  test minima, changed mapped commands/tiers/adapters, removed implementation inputs, smaller
  behavior scopes and relaxed behavior budgets are also rejected. Closed findings cannot silently
  reopen. Legitimate migrations require explicit review of the ratchet rather than a bypass flag.

These are local evidence-integrity checks, **not signed attestations or proof that a fix is causal**.
Reviewers still need to inspect test relevance, command filters, ownership, causes and the actual
patch. A broad owner-suite mapping does not establish exhaustive operation/argument coverage.

## Initial coverage is deliberately incomplete

The initial plan contains 69 configured families in 18 owner groups. Sixteen groups map existing
Rust owner suites to the contract dimension; controllers and integrations have explicit contract
gaps. Other dimensions, including live behavior, remain gaps until reviewed checks are attached.
The plan also offers offline audit/embedded-driver checks and a separate optional workspace run.
Those helper checks do not silently fill unrelated coverage cells.

Follow-through mappings add 8 reviewed dimension cells, backed by 11 narrow `--exact` native
checks (`shell-cancel-*`, `shell-steer-effects`, `edit-recovery-*`, `write-*`,
`background-cancel-fd-wait`, `eval-cancel-namespace`). Each mapped test was read in full and has
no early-return or skip branch on its asserted path. Shell and background checks are `unix`-gated
and the eval check is `macOS`/`Linux`-gated, so on a host outside those platforms the test count is
zero and the check fails `minimum_tests` rather than passing. Expanded eval concurrency and recovery
checks below are macOS-only; other hosts must supply their own evidence rather than reuse these
receipts. The write cancellation test runs sequentially; it does not establish concurrency.
Native contract tests still cannot satisfy `behavior`.

Expanded native mappings cover file read/write/edit/search boundaries, browser authorization and
cancellation, Git effects and permissions, LSP protocol settlement, DAP cleanup, notebook publication
races, IDA worker cleanup, report redaction, controller tool-offer persistence, shell recovery and
output integrity, and background/eval job settlement. These are narrow
native checks, not live integration evidence. Shared/exclusive classifications alone do not fill a
concurrency cell; draft text alone does not prove absence of external effects. Tests that silently
return when prerequisites are absent are excluded from these added dimension mappings. Unmapped
operations and dimensions remain explicit gaps; a successful non-strict gate is not completeness.

`validate` means **valid plan, not executed audit**. CI runs offline gate contracts and the
coverage/finding ratchet. It does not claim the initial plan is complete or run paid providers.
Use the strict gate below for a completeness requirement; the initial global strict gate fails
as intended. A green CI plan check is not a green exhaustive tool audit. Closure receipts are
ignored local evidence, so `validate` reports a closed finding whose receipts are absent (as in CI)
as `closed-unverified`, never `closed`; `gate` still requires and verifies them.

## Local follow-through checkpoint (2026-11-13)

Receipts under `benchmarks/tool-audit-completion-20261113/` are ignored local evidence, not
portable attestations. `final-native/run.json` records 108 passing checks;
`final-offline/run.json` records 49 audit contracts and 64 embedded-driver tests;
`final-platform/run.json` records 10,280 passing workspace tests. All 111 checks share one
unchanged source fingerprint. `coverage.json` records **83 passing cells and 61 gaps**;
`strict-coverage.json` contains the same ledger and the strict command exits 1 intentionally.
TOOL-001 has a fresh passing retained-version regression and its closure receipt is valid.

The shell cleanup regression fails before the final nonblocking probe and passes after it;
a separate assertion preserves incomplete-capture errors when a writer still holds the pipe.
Contract, isolated-worker and repair-progress fixtures use test-only workspace-scoped capture
allowances; the production 250 ms content-capture bound is unchanged. Eight final 128-thread
agent-suite runs each passed 2,639 tests (six ignored) in `final-grouped-stress/run-{1..8}.log`.

These passes do **not** close unreproduced incidents: the original grouped-verification
missing `checks` file, the one-off contract hook `interrupted` result, or the reported full-module
TCP-redirection hang. The grouped regression now asserts its nested verification result and
prints settlement diagnostics. The TCP test and complete final workspace/native shell suites
passed, but no causal repair for that historical hang is established. The orphan-cleanup fixture
now allows three seconds for startup; its original empty-output incident remains a timing
hypothesis/mitigation, not a proven production defect or causal fix.

Runtime boundaries remain explicit: live-model/provider behavior was not run (model loading
was declined); Linux sandbox execution was refused because the container host disallows the
required user namespaces (security relaxation was declined). Windows/cross-OS runtime behavior
is unverified. Earlier isolated Python/SDK/MLX runs passed, but are not live-provider evidence.
The remaining dimension gaps require their own reviewed operation-specific tests or permitted
runtime evidence; they cannot be filled by relabeling the passing owner suites.

| Owner group | Remaining dimensions |
| --- | --- |
| read | behavior |
| write | behavior |
| edit | behavior |
| search | cancellation, behavior |
| shell | concurrency, behavior |
| background | effects, behavior |
| eval | effects, permissions, behavior |
| browser | effects, recovery, concurrency, presentation, behavior |
| git | recovery, cancellation, concurrency, presentation, behavior |
| lsp | behavior |
| dap | effects, permissions, concurrency, presentation, behavior |
| ast | recovery, cancellation, concurrency, presentation, behavior |
| notebook | effects, permissions, cancellation, concurrency, presentation, behavior |
| archive | effects, recovery, cancellation, concurrency, behavior |
| ida | concurrency, presentation, behavior |
| report | effects, recovery, cancellation, concurrency, behavior |
| controllers | effects, cancellation, concurrency, presentation, behavior |
| integrations | contract, concurrency, presentation, behavior |


## Commands and evidence tiers

Run from the repository root with Python 3.11+; Rust and embedded drivers need their normal
repository prerequisites. Commands are argument arrays, not shell snippets. Only run a reviewed
manifest: it can execute programs with the invoking developer's permissions.

```sh
# No model calls; validates the inventory and runs adversarial gate contracts.
make test-tool-audit

# Existing complete offline benchmark/analysis contract suite.
make test-bench-offline

# Execute all declared offline checks into a NEW ignored evidence directory.
make audit-tools AUDIT_TIER=offline AUDIT_OUT=benchmarks/audit-offline-001

# Existing native Rust owner suites, with the repository embedded-host prerequisite wrapper.
make audit-tools AUDIT_TIER=rust AUDIT_OUT=benchmarks/audit-native-001

# Or select one check from the requested tier.
python3 examples/benchmarks/tool_audit.py run --tier rust --check edit-contracts \
  --out benchmarks/audit-edit-001

# Show fresh evidence and remaining gaps; no strict completeness claim.
python3 examples/benchmarks/tool_audit.py gate \
  --receipt benchmarks/audit-native-001/run.json

# Require every group/dimension and every finding to pass/close. Initially fails.
python3 examples/benchmarks/tool_audit.py gate --require-complete \
  --receipt benchmarks/audit-native-001/run.json

# Enforce a deliberately narrower slice without claiming global completion.
python3 examples/benchmarks/tool_audit.py gate --require-complete \
  --group edit --dimension contract --receipt benchmarks/audit-edit-001/run.json

# CI-style comparison against a full commit hash, without changing Git history.
python3 examples/benchmarks/tool_audit.py validate --base-ref "$(git rev-parse HEAD)"
```

Keep reports under ignored `benchmarks/`, not beside hashed source or fixtures. Directories,
logs and reports are created exclusively; reruns need a new path. `run.json` records each
check and its associated log. Missing programs are `unavailable`, failed/zero-test/changed-input
runs are not passes, and `run` exits nonzero if any selected check did not pass. Interrupting
an executing child stops further checks and retains completed/interrupted evidence.

Tiers stay separate:

| Tier | Meaning and limits |
| --- | --- |
| `offline` | Audit/analysis contracts and local embedded-driver fixtures; no model-quality or live-service conclusion. |
| `rust` | Reviewed native owner tests on the invoking host; individual tests may still use mocks. |
| `platform` | Explicitly selected environment-dependent checks. The optional `workspace-contracts` entry runs `make test-rust`, which needs normal sandbox/display/runtime prerequisites. It is not proof of another OS. |
| `live` | Reviewed benchmark commands which may consume model/provider quota; require `--allow-live`. The initial manifest has none. |

`--check` cannot select checks from another tier. To run an added live entry, use the Python
CLI with both `--tier live --allow-live`; the Make target intentionally does not supply live
permission. Never put credentials in the manifest, commands or committed reports. Logs and model
reports can contain private data and belong in the ignored evidence directory, not uploaded as-is.

Do not edit inputs during an evidence run. In a shared checkout, coordinate with other sessions
or run from an isolated copy of the intended source. Source drift is a reason to retain the run
as stale evidence and rerun after stabilization, not to weaken freshness validation.

## Behavior evaluation

The adapter reuses `tool_effectiveness.summarize_tools`, `reliability.py` and
`compare.checked_success`; it does not introduce another grader. To analyze an existing report
without invoking a model:

```sh
python3 examples/benchmarks/tool_audit.py evaluate benchmarks/repeated-machine.json \
  --candidate candidate --out benchmarks/audit-behavior-summary.json
```

Reports need matching verifier/task/source contracts, at least three repetitions, independently
checked outcomes and complete main-agent tool traces for baseline and candidate. Required live
check tasks/tools must be present and invoked. Correctness regressions block success even if
another task is faster. Efficiency is assessed per matched always-successful task, not by letting
one task's savings hide another's regression. Default candidate/baseline budgets are:

- tool calls and observed tokens: at most 1.10×;
- elapsed time: at most 1.25×;
- tool errors and contract errors: no increase (1.00×).

A zero baseline permits no positive increase. Unknown metrics remain unknown and make the
result inconclusive; unknown token usage is not free usage. Reviewed per-check `limits` may be
set in the manifest; the baseline ratchet prevents subsequently relaxing them. Evaluation means
**observed nonregression**, not statistical proof of improvement, cost savings or exhaustive
behavioral coverage. It does not measure tasks outside the supplied benchmark or worker traces.

A live manifest check uses `adapter: "behavior"`, `tier: "live"`, a fresh repository-relative
`report` under `benchmarks/`, `candidate`, required `tasks`/`tools`, the normal `argv`, `sources`,
`inputs`, `timeout` and `minimum_tests`. Its reviewed command must generate the existing repeated
machine-benchmark report. The runner refuses a pre-existing report and embeds the resulting
payload in its hashed log for subsequent offline validation. A synthetic adapter fixture proves
adapter behavior only; it is not live evidence.

## Adding coverage and closing findings

1. Identify the owning group and the missing operation/dimension. Add a narrowly relevant check
   with explicit tier, adapter, argument array, timeout, positive test minimum, test `sources` and
   implementation `inputs`. Include causal helper files in `inputs`, not just plan metadata.
2. Replace only the dimension's gap which that check actually tests. Keep permissions, effects,
   recovery, cancellation, concurrency and visual/presentation coverage separate. A mocked test
   does not establish live side effects; source/DOM assertions do not establish rendered appearance.
3. For a demonstrated defect, add a finding with `id`, `group`, `owner`, `status: "open"`,
   `evidence`, `cause`, `next_action` and its specific regression `check`. Preserve historical
   investigations as leads unless reproduced on the audited source.
4. Record the failing check **before** applying the fix. Keep its receipt/log. Fix the owner, not
   a parallel workaround. Prepare final finding metadata (`status: "closed"`, descriptive `fix`,
   `before`/`after` receipt paths) before the after-run so the manifest itself does not stale it.
   `run` validates the plan without requiring the future after-receipt to exist.
5. Run that same check on fixed inputs into the final `after` path. Then run `validate`/`gate`:
   they verify failing-before, passing-after, temporal order and changed inputs. An unfixed or
   evidentially incomplete finding stays open; global `--require-complete` refuses it.

No finite test inventory proves that nothing was missed. The enforceable guarantee is narrower:
missing ownership, missing dimensions, absent/stale execution and unsupported closure are visible
and cannot satisfy a requested strict gate.
