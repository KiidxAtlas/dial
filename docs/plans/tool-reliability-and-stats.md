# Plan: tool reliability and trustworthy usage statistics

Status: implemented except where an item records a reason (3.4 has no fix; live re-measurement
pending). Source analysis: `~/.dial/stats.sqlite3` (queried from a copy because the live
database was locked), the runtime tool nodes in `~/.dial/projects/*/sessions/*.jsonl`
(81.5k tool executions, 2026-10-07 → 2026-10-31) and `ARCHITECTURE.md`.

Each item lists the problem and its evidence, the cause in the code, the proposed changes, the
affected files, risks, verification and a metric to re-measure. Line numbers came mostly from
read-only scouts; each location is re-checked before it is changed, and any deviation is recorded
in the **Implementation notes** of that item.

Already fixed or misattributed, so not changed here:

- `computer is not defined` (18 errors): the hint in
  `crates/dial-computer/src/computer/js/worker.mjs` (`hintWrongGlobal`) landed on 10-23; the last
  error was on 10-23.
- `find` `grep_keywords` passed as a string (9): the coercion in
  `crates/dial-agent/src/agent/run/tool_compat.rs` was committed on 10-31 15:24; the two later
  failures are most likely from a binary built before that commit.
- `codebase_memory` errors happen only in worker sessions; the change for workers is item 0.3.

Out of scope (working well): prompt caching (Opus ≈96% of input from cache, Sonnet/Haiku ≈90%),
provider failures (1,154 fatal of 92k requests) and the `read`/`grep`/`git` error rates
(1.1–2.2%).

---

## Phase 0: make the statistics trustworthy

### 0.1 Tests write telemetry into the real `~/.dial`

- **Evidence:** 12,736 requests have `project=''`, nearly all under the model names `test`,
  `default_model`, `account-model` or `x`. 9,443 tool calls have `project=''`, including all
  1,009 `fixture_lookup` calls and 8,251 `read` calls.
- **Cause:** `Config::default()` sets `user_dir` to the real `~/.dial`
  (`crates/dial-llm/src/config/types.rs`), stats are on by default (`config/stats.rs`), and
  telemetry writes whenever `user_dir` is absolute (`crates/dial-agent/src/telemetry.rs`). Tests
  such as `crates/dial-agent/src/agent/workers/child/tests.rs` build agents from
  `Config::default()` without the `hermetic` flag that `telemetry::tool` already honours.
  `fixture_lookup` is defined in those tests.
- **Changes:**
  1. Test configs get an isolated `user_dir` (or `hermetic`) through one shared mechanism rather
     than per-test edits.
  2. Second guard: telemetry drops events whose `project` is empty; a real session always has one.
  3. Cleaning the polluted rows already in the database is a one-off, opt-in data cleanup, not
     automatic.
- **Files:** `crates/dial-llm/src/config/types.rs`, `crates/dial-agent/src/telemetry.rs`, test
  helpers.
- **Risk:** tests that assert on telemetry in the real directory must move to a temp directory, as
  `telemetry/tests.rs` already does.
- **Verify:** a test asserting the shared test agent writes nothing outside its temp directory;
  `make test-module MOD=agent::workers::child`, then `make test-rust`.
- **Metric:** `select count(*) from requests where project=''` must not grow after a full
  `make test-rust`.
- **Implemented — deviation:** re-reading `telemetry.rs` showed a different root cause.
  `project_key` used `config.state_dir.file_name()`, but `state_dir` is the model learning
  directory (`<user_dir>/projects/<name>-<hash>/models/<digest>`, `dial-llm/src/config/mod.rs`),
  so runtime rows were keyed by a model digest and never joined the session directories. For
  `Config::default()` it is empty, which produced the `project=''` rows. `project_key` now takes the
  path component directly under `<user_dir>/projects` and returns `""` when no project is loaded.
  `Route::from_config` sends nothing to stats when the key is empty, which covers change 2 and makes
  every unloaded test config inert without editing each test (change 1). `config/types.rs` was not
  changed. Polluted rows are left as they are (change 3, opt-in). Test:
  `telemetry::tests::stats_are_keyed_by_project_directory_and_skip_unloaded_configs`.

### 0.2 `check` counts a still-running check as an error

- **Evidence:** 120 of 231 `check` calls in sessions have status `error`; about 48 begin with
  `unverified: … command is running as background job`; 14 begin with `passed:` (a sibling check
  was unverified); only 8 begin with `code_failed:`.
- **Cause:** `crates/dial-agent/src/agent/verify/check_tool.rs:340` sets
  `is_error: !all_passed || refused.is_some()`. A check that outlives the shell's auto-background
  threshold returns `ToolOutput::ok("…background job…")`
  (`crates/dial-tools/src/tools/exec/shell.rs`) with no process outcome, so
  `verify/verification.rs` classifies it `Unverified`.
- **Changes:**
  1. Run checks in the foreground with the check's own timeout; a check that has not finished
     verifies nothing.
  2. `is_error` only for `CodeFailed`, `TimedOut`, `EnvironmentUnavailable`, `PermissionDenied`
     or a refused repro.
  3. `Unverified` remains "not passing" for the completion gate.
- **Files:** `verify/check_tool.rs`, `verify/checks.rs`, `verify/verification.rs`.
- **Risk:** long checks hold the turn; bounded by the existing check timeout, reported honestly as
  `TimedOut`.
- **Verify:** a test in `verify/checks/tests.rs` or `verify/check_tool/tests.rs`;
  `make test-module MOD=agent::verify`.
- **Metric:** `check` error rate (≈51% today) should fall toward the `code_failed` share.
- **Implemented — deviation:** change 1 as planned. Focused checks (`verify/checks.rs`
  `run_focused_check`) now go through a new `Agent::run_foreground_tool`
  (`agent/run/tool_runner.rs`). It shares the off-thread runner with `run_background_tool` but sets
  `shell.auto_background_secs = 0` for that call, so the check's own timeout bounds it.
  Change 2 was **not** made. Four existing tests (`check_tool/tests/incremental.rs`,
  `checks/tests.rs::failed_contract_capture_is_unverified_in_gate_and_check_tool`) encode the
  deliberate contract that every non-passing check (unverified, cancelled) is a failed
  observation for the model. The measured inflation came from auto-backgrounding, which change 1
  removes at the source, so `is_error` semantics stay as they are (change 3 holds trivially). Test:
  `check_tool::tests::a_check_outlasting_the_auto_background_threshold_finishes_in_the_foreground`
  (fails with the override removed, passes with it). `agent::verify` 164/164 pass.

### 0.3 `codebase_memory` errors happen only in workers

- **Evidence:** top-level sessions: 51 calls, 0 errors. All 396 errors come from 362 sessions
  without a top-level transcript (workers), 1–3 calls each.
- **Cause (partly inference):** `crates/dial-agent/src/agent/workers/subagents.rs` tells every
  worker that has the tool to "Save reusable architectural findings … with codebase_memory
  remember", read-only scouts included. `remember`
  (`crates/dial-tools/src/tools/knowledge/codebase.rs`) rejects unindexed or changed sources,
  revision mismatches and out-of-bounds input, and defaults an omitted `expected_revision` to 0,
  which always fails on an existing topic.
- **First step:** pull the error text for those calls from worker transcripts.
- **Changes:**
  1. Workers are told to recall; saving belongs to the parent after integration.
  2. When `expected_revision` is omitted and the topic exists, the error gives the current
     revision in a ready-to-copy form. An explicit stale revision is still rejected.
- **Risk:** schema-description edits change the contract hash; keep to error text and prompt text.
- **Verify:** tests in `knowledge/codebase/tests.rs` (existing topic without revision, stale
  revision) and a prompt assertion in `workers/subagents/tests.rs`.
- **Metric:** `codebase_memory` error rate in worker sessions.
- **First step done — deviation:** worker transcripts (`projects/*/subagents/*/*.jsonl`, message
  format) were sampled. Failing `remember` results, by class: `codebase finding limit reached`
  ≈282, `cannot be fingerprinted` (`.sql`, `.vue`, `Dockerfile`, files over 256 KiB) ≈60,
  `codebase cache belongs to another workspace` 24 (all 10-20 → 10-22, before isolated copies
  dropped root-bound tools, so already fixed), `$.sources` over 8 items 8, changed or unindexed
  source ≈22. No `expected_revision` mismatch was observed, and the existing error already quotes
  the current revision ready to copy. The `dial` project's `codebase.sqlite3` holds exactly
  `MAX_NOTES` = 128 findings, so every new topic from any worker fails.
- **Implemented:** change 1: the worker prompt (`workers/subagents.rs`) now says to recall and
  not save; findings go into the answer for the parent. Change 2 was retargeted at the observed
  cause. The limit refusal (`knowledge/codebase.rs`, new `limit_reached`) gives the limit and lists
  up to five findings whose cited sources no longer match the index, as exact `forget` arguments
  (`{"expected_revision":N,"topic":"…"}`). The revision-mismatch text is unchanged. Updates to
  existing topics are still allowed at the limit. No schema text changed, so no contract-hash
  impact. Fingerprint refusals are unchanged (deliberate source-type policy). Tests:
  `codebase::tests::a_full_cache_names_stale_findings_to_forget_and_still_accepts_updates`, an
  assertion in `subagents::tests` (prompt). `tools::knowledge::codebase` 67 passed;
  `agent::workers::subagents` 45 passed.

### 0.4 The session index is stale and worker sessions aren't attributed

- **Evidence:** the `sessions` table was last synced around 10-25; 466 of 1,101 session files are
  missing. `tool_calls` covers 3,218 sessions; only 199 match a session row.
- **Cause:** `sync_sessions` runs only from `/stats` and the stats server; telemetry rows do not
  record whether a session is top-level or a worker.
- **Changes:** an additive `kind` column (top-level or worker) on `requests` and `tool_calls`;
  incremental sync at session end. Cost: subscription transports report no cost, so `gpt-6.1-sol`
  shows blank next to Opus's $96. Trace the pricing code and show "subscription" rather than a
  blank.
- **Files:** `crates/dial-agent/src/stats/`, `crates/dial-agent/src/telemetry.rs`, the session-end
  path.
- **Verify:** extend `stats/tests.rs` sync tests.
- **Metric:** stats and on-disk session counts agree; worker and top-level error rates reportable
  separately.
- **Re-verified:** worker usage rows carry the child's private session id (`children.namespace`,
  `workers/child.rs`). That id appears in no file name: worker transcripts are
  `subagents/<parent-session>/task-N-<ms>.jsonl`. Of the 1,899 distinct non-empty-project sessions
  in `tool_calls`, 538 match a top-level transcript; the rest are workers or deleted sessions,
  and rows alone cannot tell them apart. `telemetry::shutdown` runs only on the restart path
  (`dial-app/src/app/mod.rs`), so "session end" is placed after `shell.finish()` instead.
- **Implemented — deviation:** instead of a `kind` column on two tables, there is an additive
  keyed table `worker_sessions(session, agent, ts)`. It is fed by a new `telemetry::worker_session`
  event from the single child-session creation point (`workers/child.rs` after
  `ChildBackends::new`), and recorded by `stats::Db::record_worker_session` (first write wins).
  Queries split usage with `session IN (SELECT session FROM worker_sessions)`, which needs no
  migration of existing tables and no change to the many request/tool call sites. The OTLP
  exporter ignores the event. `stats::sync_after_session` (best effort, only with stats enabled)
  runs when the interactive shell finishes. Test:
  `stats::tests::worker_sessions_separate_worker_usage_and_session_end_syncs_the_index`.
- **Subscription cost label — implemented (follow-up):** no `telemetry::request` call site
  changed, so `agent/compact/compaction_runtime.rs` (another session's uncommitted file) is
  untouched. Every reply passed to telemetry comes from `Backend::chat_with_control_events`
  (`dial-llm/src/llm/provider.rs`), which already decides `llm::usage::Billing`; it now stores it
  on the reply (`Reply::billing`, `None` for fixtures). `telemetry::request` copies it into
  `Request::billing`. Stats add two columns through the existing migration: `requests.billing`
  (`api`/`local`/`subscription`) and `request_hourly.subscription` (count, default 0).
  `Totals::subscription_requests` feeds `cost_text`: `subscription` when every successful request
  was subscription-billed, `unpriced (N subscription)` or `$x (p of n priced, N subscription)` when
  mixed. The `/models` detail line shows `subscription` instead of `avg cost -`. Rows recorded
  before this stay `unpriced`. Tests:
  `stats::tests::subscription_requests_are_labelled_instead_of_unpriced_and_legacy_rollups_migrate`,
  `telemetry::tests::the_replys_billing_reaches_the_stats_row`. Docs:
  `docs/share-stats-telemetry.md`.
- **Re-measure:** after new sessions,
  `SELECT session IN (SELECT session FROM worker_sessions) worker, tool, count(*), sum(error)
  FROM tool_calls WHERE ts > <deploy ms> GROUP BY 1,2`; session coverage: `SELECT count(*) FROM
  sessions` against `ls ~/.dial/projects/*/sessions/*.jsonl | wc -l`. Worker rows recorded
  before this change stay unattributed.

---

## Phase 1: cheap fixes for the most frequent wasted turns

### 1.1 `edit` hashline format errors

- **Evidence:** 528 `edit` errors in sessions: 45 empty `PUT` bodies (10-08 → 10-31), 26 sections
  with no operations, 6 empty tags, 48 retry-barrier hits (since 10-28) and 19 recovery-checkpoint
  hits.
- **Cause:** `crates/dial-tools/src/tools/edit/hashline.rs`: a blank line ends a `PUT` body with
  zero rows (deletion intent or a blank line without `+`); the end-of-input variant gives no hint;
  the empty-tag hint is circular; "no operations" names the path but not the fix.
- **Changes:** each error repeats the offending operation and its corrected form (`CUT N.=M` for
  deletion, a lone `+` for a blank line); for an empty tag, quote the path's current tag from the
  latest read witness (no silent resolution); the retry-barrier text quotes the original error.
  Not recommended yet: treating an empty `PUT` as a deletion.
- **Risk:** error text does not change the contract; the description in `tools/mod.rs` does.
- **Verify:** cases in the error table of `hashline/tests.rs`;
  `make test-module MOD=tools::edit::hashline`.
- **Metric:** `edit` error rate (7.4%) and share of retry-barrier hits.
- **Implemented:** `hashline.rs` `empty_body_error` repeats the `PUT` line and suggests
  `CUT <same target>` (or removing an insert); "no operations" says to move the operations under
  the header or drop it. Deviation: the parser now admits an empty tag and `load_job` refuses it,
  because only there is the edit store available to quote `[path#TAG]` from the latest read (or
  say the file changed / was never read). The retry barrier (`agent/run/engine.rs`) keeps the
  first line of the original error per withheld call and quotes it. Tests:
  `hashline/tests.rs` `empty_put_bodies_and_sections_name_the_corrected_operation`,
  `an_empty_tag_quotes_the_latest_read_without_applying_the_edit`; extended
  `turn/reliability_tests.rs` `a_known_tool_typed_into_bash_…`.

### 1.2 `message` sent to finished workers (114 cases)

- **Cause:** `Team::retire` (`crates/dial-agent/src/agent/workers/mailbox.rs`) deletes the worker
  and keeps no record; the model often already received the result through `task wait`.
- **Changes:** a bounded tombstone map (id → label, final status) written in `retire`; a send to a
  tombstoned worker returns a non-error receipt; unknown ids list live recipients; a 32-hex peer id
  gets a one-line `peers` example.
- **Verify:** a tombstone case in `mailbox/tests.rs`.
- **Metric:** `message` error rate (4.5%).
- **Implemented:** `mailbox.rs` keeps a bounded tombstone queue (64; id, label, final status)
  written by `Team::retire(id, status)` (all three callers in `live_tasks.rs` pass the status). A
  send to a tombstoned id or label returns a `finished` receipt row (`worker_status`, note pointing
  at `task`), delivers nothing and is not an error; a live label wins over a tombstone. Unknown
  recipients now list the live roster. The 32-hex peer-id hint already existed (no change). The
  `message` description was updated; descriptions are excluded from the contract hash
  (`contracts.rs` `parameter_shape`), so no `ADDITIVE_PROPERTIES` entry is needed. Tests:
  `mailbox/tests.rs` `unknown_recipients_fail_with_the_live_roster_and_finished_ones_get_a_receipt`,
  `tombstones_are_bounded_and_a_live_label_wins`; updated
  `default_offer_tests.rs` `the_parents_message_tool_reports_receipts_and_marks_finished_workers`.
  Deviation: the out-of-band `LiveTasks::send` (user `/message`, RPC `task_control.send`) still
  returns an error for a finished worker ("… has already finished (status); the message was not
  delivered"), because a human or client caller reads `Ok` as delivered; the integration test
  `out_of_band_live_cancellation_unblocks_pending_approval_without_mutation` keeps that contract.

### 1.3 Argument and scope errors

| Error | Cause | Change |
|---|---|---|
| `grep`/`glob` "must not enter .git or workspace scratch" (38) | `fs/search.rs` rejects a `path` with a `.git` or `.tmp` component; the description says only "never listed". | Keep the refusal; name the alternative in the error; state the rule in the `path` description. |
| `grep` unexpected `limit` (4) | confusion with `find`; alias table in `run/tool_compat.rs`. | Alias `limit` → `max_results`. |
| `grep` `context` above the maximum (5) | offered schema has no `maximum`; runtime clamps silently. | Find the schema source first; make schema and clamping agree. |
| `find` `grep_keywords` as a string (9) | already coerced (10-31). | Re-measure after rebuild. |
| `peers` scope "must be a canonical relative path" (19) | `dial-storage/src/storage/coordination.rs` rejects `./x` and `x/`. | Normalize `.` and trailing slashes; still reject `..` and absolute paths. |
| `task` empty `solution_space` | vague description in `workers/subagents.rs`. | Add an example (contract hash). |
| `computer` `args` at top level | `chain` item shape not shown. | Add a `chain` example. |
| `computer is not defined` (18) | already fixed 10-23. | None. |

- **Verify:** `run/tool_compat/tests.rs`, `fs` search tests, coordination tests; integration
  contract-identity tests; `ADDITIVE_PROPERTIES` where needed.
- **Metric:** count of `invalid tool call:` results per tool.
- **Implemented:** `fs/search.rs` refusal quotes the path and names `read`/`bash git` as
  alternatives; `grep`/`glob` `path` descriptions state the rule. `run/tool_compat.rs` aliases
  `grep` `limit` → `max_results` (only when `max_results` is absent). `coordination.rs` drops a
  leading `./` and trailing `/`; `..`, absolute, `.`, `//` and mid-path `./` are still refused and
  the error names the canonical form. `task` `solution_space` and `computer` `chain` descriptions
  gained examples. Deviation (grep `context`): already consistent in source: the schema has no
  `maximum` and runtime clamps to 10 (`search.rs` `MAX_GREP_CONTEXT`, test
  `oversized_grep_context_is_capped_at_runtime_not_rejected`). The 19 session hits (10-06 → 10-15,
  one 10-31) came from older binaries; the installed `dial 0.2.9 (5b04081d)` still offers the old
  schema, so the fix lands on rebuild. Only descriptions changed, which `parameter_shape` excludes
  from contract identities, so no `ADDITIVE_PROPERTIES` entry. Tests: `tool_compat/tests.rs`
  `grep_limit_is_read_as_max_results_unless_both_are_given`; `search/tests.rs`
  `glob_patterns_sort_and_cap` (refusal text); `coordination/tests.rs`
  `unique_instances_canonical_scopes_and_layout_isolation` (normalization cases).

---

## Phase 2: sandbox and environment friction

### 2.1 `lsp` can't find rust-analyzer (51/145 fail)

- **Cause:** `crates/dial-tools/src/tools/code/lsp/servers.rs` searches `PATH`, Dial's tool
  directories and project bins; here `rust-analyzer` exists only under
  `~/.rustup/toolchains/*/bin` and Homebrew's `rustup` installs no proxy.
- **Changes:** fall back to `rustup which rust-analyzer` (respects `rust-toolchain.toml`) when
  `PATH` misses; list the searched directories in the error; check the `pyright` shim separately.
- **Risk:** starts a toolchain binary; same approval path as other LSP startup.
- **Verify:** fake `RUSTUP_HOME` test in `lsp/servers/tests.rs`.
- **Metric:** `lsp` error rate.
- **Implemented:** `find_program` falls back to `rustup which rust-analyzer`, run in the server
  root with `RUSTUP_AUTO_INSTALL=0` and stdin closed, when no directory has the program
  (`toolchain_program`, shared helper `command_path`). The not-found error now lists up to
  eight searched directories (`searched`). Pyright, checked separately: the pyenv shim
  `~/.pyenv/shims/pyright-langserver` is on PATH, but the active Python (3.12.15) does not have
  the command (only 3.14.5 does), so the shim exits and the server "exited while handling
  initialize". Shims under a pyenv root are now resolved with `pyenv which` and skipped when
  that fails, so a later directory wins or the server counts as missing with install hints.
  Tests: `servers/tests.rs` `rust_analyzer_falls_back_to_rustup_which_in_the_server_root`
  (deviation: a fake `rustup` on the searched dirs that checks cwd and `RUSTUP_AUTO_INSTALL`
  instead of a fake `RUSTUP_HOME`), `pyenv_shims_resolve_to_the_active_version_or_are_skipped`.
  `cargo test -p dial-tools --lib -- tools::code::lsp` 98 passed; clippy clean.

### 2.2 `dep_source` refused under `~/.cargo/registry` (11)

- **Cause:** strict policy in `crates/dial-tools/src/tools/knowledge/deps.rs` allows reads only
  inside workspace roots.
- **Change:** a read-only grant for `dep_source` alone over the resolved
  `CARGO_HOME/registry/src` (and `GOMODCACHE`); `deny_read` still wins.
- **Risk:** sandbox-policy change.
- **Verify:** strict-mode tests in the `deps` tests: registry allowed, other home paths refused.

- **Implemented (user approved the policy change):** `deps.rs` `authorize` takes the resolved
  `Caches`; under Strict a package whose canonical base lies in `CARGO_HOME/registry/src`,
  `CARGO_HOME/git/checkouts` or the Go module cache (`Caches::contain`) is readable by
  `dep_source` only. `deny_read` is now checked first under both policies, and the existing
  checks still apply before it: root inside base (symlinks not followed out), protected Dial
  storage. Other out-of-workspace paths keep the `request_directory` refusal. Deviation: the
  grant also covers cargo git checkouts, which the resolver already reads for git dependencies
  and which belong to the same locked-source cache. `docs/tools.md` updated. Tests:
  `deps/tests.rs` `caches_outside_the_workspace_follow_the_sandbox_read_policy` (strict
  registry read allowed; denied path refused under Strict and Dev; home outside the caches not
  granted), `the_strict_grant_does_not_cover_home_paths_outside_the_caches` (a symlinked
  package that leaves the cache is refused). `cargo test -p dial-tools --lib --
  tools::knowledge::deps`: 8 passed; clippy clean.

### 2.3 Other gates

- `bg` port readiness denied (9): probing a loopback port the job itself opened should not need
  network approval; restrict to 127.0.0.1 and ports the job owns.
- `web_fetch` to localhost: intentional SSRF guard; the error should point to `browser` or
  `bash curl`.
- Reading other sessions' JSONL: refusal is correct; the error should point to `session_search`.
- **Implemented (user approved the bg policy change):**
  - Deviation, `bg`: the plan's premise holds only for unsandboxed jobs. Under the macOS
    sandbox a job without network gets `(deny network*)`, which also refuses binding a
    loopback listener (`sandbox-exec` with that rule: `bind(127.0.0.1)` → `EPERM`; unsandboxed
    the same bind succeeds), so a probe could never succeed there. Session text: 117 × "denied by
    the workspace's network policy", 32 × "launched without network permission" (all
    sessions, including older binaries). Change: `background/mod.rs` `port_probe_refusal`
    replaces the three gates; an unsandboxed job may be probed on a loopback address
    without network permission (lsof ownership is still required before the TCP probe);
    non-loopback still needs network; a sandboxed job without network gets an error saying the
    listener can't open and suggesting `ready.log`. `ready.port` description updated
    (description-only).
  - `web/guard.rs`: the SSRF refusal now names `browser` and `bash` with curl for local
    servers.
  - `tools/mod.rs` `Workspace::protected_refusal`: a refused `sessions/*.jsonl` path in
    protected Dial storage (inside or outside the workspace) points to `session_search`;
    other protected paths keep "enters protected Dial storage".
  - Tests: `background/tests.rs` `loopback_port_probes_of_unsandboxed_jobs_need_no_network_permission`,
    `an_unsandboxed_job_without_network_becomes_ready_on_its_own_loopback_port` (real lsof +
    python3 listener, macOS); `tools/tests.rs`
    `protected_session_transcripts_point_to_session_search`; existing `web/tests.rs` refusal
    test still passes. `tools::exec::background` 73 passed; `tools::tests tools::net::web
    tools::fs` 355 passed.

---

## Phase 3: output quality and tool flow

### 3.1 `bash` results bury the failing line

- **Evidence:** 1,272 errors; 62 are a bare `(no output) [exit code: N]`; cargo failures start with
  pages of `Compiling` lines.
- **Cause:** `crates/dial-tools/src/tools/exec/minimizer.rs` filters but does not reorder, and only
  when savings clear its thresholds and full output is recoverable.
- **Changes:** for failed cargo/npm/test runs, a short digest first (first error lines, file:line,
  exit code), full output kept in the artifact; for empty output with nonzero exit, the
  conventional meaning for known tools (grep 1 = no match); `is_error` stays truthful.
- **Verify:** minimizer tests on cargo-failure fixtures.
- **Metric:** failed `bash` followed by an identical rerun or by an artifact read.
- **Implemented:** `minimizer::failure_digest` (recognized build/test commands via the existing
  `filter_for`; output ≥ 4 KiB whose first error line starts ≥ 1 KiB in; up to 12 error-head and
  following `--> file:line` lines) and `minimizer::silent_exit_meaning` (exit 1 with no output
  for grep/rg/ag, `git grep`, `git diff --quiet|--exit-code`, diff/cmp, test/`[`, pgrep, which).
  `exec/shell.rs` `run_bash` prepends the digest to failed (not cancelled or timed-out) runs and
  appends the meaning after `[exit code: 1]`; `is_error` is unchanged. Tests:
  `minimizer/tests.rs` `a_cargo_failure_buried_under_progress_gets_a_digest_of_its_first_errors`,
  `silent_exit_codes_with_a_conventional_meaning_are_explained`; `shell/tests.rs`
  `failed_builds_lead_with_a_digest_and_silent_grep_misses_are_explained` (sandboxed, real run).
- **Deviation:** the digest is not filtered output: the full (minimized) output still follows it
  inline as before, so nothing is hidden; `bg` output is not digested (its log is read
  incrementally, where a front digest has no fixed "start").

### 3.2 `browser` called without an open tab (38)

- **Cause:** `crates/dial-tools/src/tools/net/browser/driver.cjs` raises the error correctly;
  unknown whether `open` was skipped or tab state lost on driver restart.
- **First step:** sample what came before each call.
- **Change:** include the current tab list in the error; fix restarts if they are the cause.
- **Sampled (deviation from "fix restarts"):** in 20 transcripts containing the error, the
  browser actions just before it were `navigate`, `screenshot`, `run`, `emulate`, `reload` and
  similar, with no preceding `open`; the model skipped `open`. No restart loss was evidenced, so
  restart handling is unchanged.
- **Implemented:** `driver.cjs` `tabFor` errors now carry the open tab list (`openTabList`, up
  to 8 `id url` pairs): "Multiple browser tabs are open; pass an explicit tab id (open tabs:
  …)" and "Browser tab N is not open (open tabs: …)". With no tabs, both errors say tabs do not
  survive a browser restart or close. Test: `driver.test.cjs` `missing tab selects one existing
  tab but never guesses among several` (extended); `node --test …/driver.test.cjs` 16/16 pass.

### 3.3 Deferred tools called before they're loaded (24, mostly `image_gen`)

- **Cause:** `crates/dial-agent/src/agent/run/turn/step.rs` loads the schema but refuses to execute
  a call whose schema the model never saw.
- **Recommendation:** validate arguments against the newly loaded schema and, if valid, run the
  call through normal approval in the same round.
- **Verify:** `run/turn` tests for valid and invalid arguments.
- **Implemented (user-approved):** `step.rs` (compatibility block ~1659–1735) still loads an
  eligible catalog tool the request did not offer (not in planning, plan mode, hermetic turns or
  under a recovery barrier), then adds only that tool's name and schema to this round's offer
  (the request's narrowed offer is not widened), normalizes the call's arguments against the
  new schema, and lets the normal round validate it. A valid call runs through the usual
  approval and sandbox path in the same round; an invalid one is refused by `round.rs` (~353)
  with "X was loaded this round, but these arguments do not match its schema (…); this call
  was not executed" plus the parameters schema. The assistant message records the call against
  the loaded schema, so its contract identity is present on resume. Tests:
  `turn/reliability_tests.rs` `a_valid_call_to_an_unloaded_deferred_tool_runs_in_the_same_round`,
  `an_invalid_call_to_an_unloaded_deferred_tool_is_refused_with_its_schema` (both with `git`,
  deferred by default); `cargo test -p dial-agent --lib -- unloaded_deferred_tool` 2 passed.
- **Metric:** `tool_form_failure` records of kind `unknown_tool` for catalog tools per session.

### 3.4 Linker warning `__eh_frame section too large` (16)

- **Change:** try `-C link-arg=-Wl,-no_compact_unwind` for macOS dev builds behind a
  target-specific config; check backtraces; `make preflight-quick`.
- **Tried, rejected (not implemented):** the warning reproduces on a debug link of the `dial`
  binary (`cargo build --bin dial`). Linking with the flag (`cargo rustc --bin dial -- -C
  link-arg=-Wl,-no_compact_unwind`) removes it, but the backtrace check fails: a minimal
  `rustc -g -C link-arg=-Wl,-no_compact_unwind` program aborts on its first panic with "fatal
  runtime error: failed to initiate panic, error 5" (`catch_unwind` never returns), while the
  same program without the flag unwinds and prints the full backtrace. Without compact unwind
  info the macOS unwinder cannot start a Rust panic here, so the flag would turn every
  recoverable panic (worker isolation, `catch_unwind` in tools) into an abort. No config was
  added. The warning is about exception-handling performance only, so a harmless warning is
  better than a broken unwinder.
- **Alternatives tried (follow-up), none kept:**
  - Measured on the `dial` debug binary (arm64): `__eh_frame` is 0x1498f74 bytes (21.6 MB), above
    the 16 MB limit; `__text` is 96 MB.
  - `debug = "line-tables-only"`: already the `[profile.dev]` setting (dependencies use
    `debug = false`), and the warning appears with it, so it is no fix.
  - `split-debuginfo = "packed"` (`CARGO_PROFILE_DEV_SPLIT_DEBUGINFO=packed`): `__eh_frame`
    0x1495a30, warning still printed.
  - `debug = false` (`CARGO_PROFILE_DEV_DEBUG=false`, fresh target dir): `__eh_frame` 0x1497c80,
    warning still printed. A synthetic 4,000-function `rustc` program agrees: the same
    `__eh_frame` at `debuginfo=0` and `line-tables-only` (252,596 B), 304 B more at
    `debuginfo=2`. Debug info does not affect unwind tables, so no debug-info setting can bring
    21.6 MB under 16 MB.
  - A minimizer filter for this one warning was not adopted: `AGENTS.md` forbids suppressing
    warnings, and the linker would still print it.
  - Not tried: raising dev `opt-level` (shrinks code, but changes every debug build and its
    compile time) and `panic = "abort"` (breaks `catch_unwind`). The warning only concerns
    exception-handling performance, so it is left as is.

---

## Phase 4: long sessions and compaction (measure first)

- **Evidence:** Opus sessions average 141 tool calls (max 1,363); 350 compactions observed.
- **Plan:** per compaction, measure re-reads of files read before it, repeated failed calls and
  time to the next useful edit; change `agent/compact/` only when the data shows a problem.
  Depends on 0.4.
- **Measured (2026-10-31, all `~/.dial/projects/*/sessions/*.jsonl`, read-only script
  `.tmp/p4.py`):** 712 compactions with at least 40 tool calls before them and 5 after. For the
  40 calls after each compaction compared with the 40 before it (control):
  - re-reads of files already read earlier in the session: 1,943/26,647 calls (7.3%) after vs
    1,886/28,480 (6.6%) before;
  - exact repeats of an earlier failed call (same tool and arguments): 21 (0.1%) after vs 17
    (0.1%) before;
  - tool calls to the next successful edit/write: median 7 after vs 6 from the control point;
    wall time from the compaction to that edit: median 74 s, p90 876 s; 40 compactions had no
    later edit.
- **Decision:** no `agent/compact/` change. The post-compaction penalty is about 0.7 points of
  re-reads and one tool call before the next edit; repeated failures do not rise. That is not
  enough of a problem to justify editing peer-owned compaction code. Limits: the control is the
  same session's previous window, so tool mix may differ; worker transcripts (`subagents/`) are
  excluded; seconds include approval and model latency.

---

## Sequencing

```diagram plan order
Phase 0 (0.1 test isolation, 0.2 check status, 0.4 sync/kind) -> trustworthy baseline
baseline -> Phase 1 (edit errors, message tombstones, aliases/scopes)
0.3 sample worker errors -> 0.3 fix
Phase 1 -> Phase 2 (lsp rustup, dep_source grant, bg loopback)
Phase 2 -> Phase 3 (bash digest, browser sampling, deferred-tool exec, linker)
0.4 -> Phase 4 (compaction measurement, then changes)
```

Tool-description changes from 1.3, 3.3 and 0.3 land together so the contract hash and
`ADDITIVE_PROPERTIES` are handled once. Every phase: `cargo fmt --check`, `make lint`,
`make test-integration`; a `CHANGELOG.md` `[Unreleased]` entry and matching `docs/*.md` updates.

**Verification of the integrated change set (follow-up).** Run in a HEAD-based worktree that
holds only this plan's changes, so other sessions' uncommitted work cannot affect the result
(the main checkout's `dial-agent` lib and tests now pass `cargo check` as well):
`cargo fmt --check` ok; `make lint` ok; `make test-rust` (whole workspace, including
the integration binary: 344 passed, 3 ignored) ok with no failures; `make test-drivers` 32/32;
`scripts/preflight.sh --quick --in-place` passed (fmt, clippy for Linux and host, musl x86_64 and
aarch64, Windows, slim, cargo-deny). Not run: full `make preflight`, because it checks the
committed tree and these changes are uncommitted; live tiers (`make test-smoke`,
`make test-mlx`).

**Still open:**

- Re-measurement below. It needs a rebuilt `dial` installed (the installed binary is 0.2.9 from
  before these changes) and about a week of real use.
- Historical rows written by tests before 0.1 (12,852 `requests` rows with `project = ''`) stay
  in `~/.dial/stats.sqlite3`, by the user's choice. Queries filter them with `project <> ''`.
- Live checks not possible here: `go` is not installed, so the Go module cache grant (2.2) is
  covered only by unit tests. The pyright shim fallback (2.1) was checked against the real
  environment (`pyenv which pyright-langserver` fails for the active 3.14.8, so the shim is
  skipped), but no full LSP session was run.

## Re-measurement

Query a copy (`cp ~/.dial/stats.sqlite3* "$TMPDIR"/`) after a week of use:

```sql
-- per-tool error rate, real sessions only
SELECT tool, count(*) n, sum(error) err, round(100.0*sum(error)/count(*),1) pct
FROM tool_calls WHERE project <> '' GROUP BY tool ORDER BY n DESC;
-- test pollution must not grow
SELECT count(*) FROM requests WHERE project = '';
```

Session-text metrics (edit message classes, message tombstones, scope errors) come from the
runtime tool nodes in `projects/*/sessions/*.jsonl` (`type=runtime`, `node.kind=tool`,
`node.result.execution.status`, `node.result.text`).
