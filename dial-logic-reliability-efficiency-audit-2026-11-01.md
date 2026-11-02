# Dial audit, part 2: logic, reliability and efficiency (2026-11-01)

**Scope:** `KiidxAtlas/dial` at commit `21c0022`. Part 1 (security, practices, health) is a separate report; nothing from it is repeated here.

**Method:**
- Six read-only reviews ran in parallel, one per subsystem: turn loop, workers and verification, LLM transport, storage and journal, tool hot paths, and UI/app loop.
- I took real measurements myself: incremental build timings, startup time, the repo's opt-in performance probes, a `perf` CPU profile of a tool-heavy turn, and one temporary benchmark (reverted afterwards).
- Findings marked **[verified]** were re-read in the source by me after the reviewer reported them. Findings marked **[measured]** come with numbers from this container.
- Every other finding comes from the reviewers, with file:line evidence. Some include small Python reproductions, which are noted. No fix has been applied.

**Caveat on numbers:**
- Unless a figure says otherwise, it comes from a `dev` (debug) build on 4 cores.
- Absolute times in a release build will be lower. Ratios and scaling behaviour carry over.
- Reviewer cost estimates are estimates, not measurements.

---

## 1. Verdict

The core logic is careful where it matters most. Every tool-call id is answered exactly once and in order. Loop bounds hold. The prompt-cache prefix is stable. Usage isn't double-counted across retries. Cancellation is checked at ≤100 ms. Edit transactions roll back cleanly. Worker merges protect concurrent user edits.

The defects come in four recurring patterns:

1. **A limit applied to one kind of thing but not its sibling.** Worker bodies are capped but tool bodies aren't. Exact-match passes skip the ambiguity count. Accounts are pinned on one transport path but not another.
2. **A per-item error turns into a session-wide latch.** One oversized journal frame, one oversized terminal frame, or one rotated credential each disable the whole session until restart.
3. **Classification tables are incomplete.** Unknown provider failure codes and finish reasons default to "retry" or "complete".
4. **Derived data is recomputed from scratch on every request, frame or tick,** when it could be cached by immutable key. This is the dominant efficiency problem, and it compounds: per-request work grows with session length, so total cost is quadratic over a long session.

| Area | Grade | Summary |
|---|---|---|
| Turn-loop logic | B+ | Ordering and bounds are sound. Notes are lost on discarded replies, the goal stall detector is blind to batched calls, and the prewalk handoff bypasses the route installer. |
| Edit correctness | B- | Hashline and transactions are solid. The patch engine can silently edit the wrong occurrence or revert indentation. |
| Provider reliability | B- | Sound retry and usage plumbing. Credential-hashed replay scopes break sessions on token refresh, and several failure and finish codes are misclassified. |
| Persistence reliability | B | Good crash discipline. One oversized frame latches the journal, and a healed torn tail stays "corrupt" forever. |
| Worker isolation | C+ | The merge transaction is sound. Backend selection, snapshotting and restore have several real gaps (one reproduced in this container). |
| Efficiency | C+ | Startup is fine and scheduling works. There are O(history) passes per request, a markdown streaming cache that is skipped for most code replies, constant idle rescans, and a dev-loop rebuild that is 2.4× larger than it needs to be. |

---

## 2. Measurements

| What | Result |
|---|---|
| Startup, `dial --version` / `--help` (debug) | 14 ms / 20 ms. No concern. |
| No-op rebuild of the binary | 0.4 s. Clean. |
| Rebuild after touching one `dial-ui` file **[measured]** | **27.4 s, of which about 16 s is avoidable.** `dial-tools` (9.3 s), `dial-agent` (8.2 s) and `dial-computer` (2.2 s) recompile even though they sit *below* `dial-ui` and didn't change. With only the necessary crates rebuilt, it would take about 11.5 s, so the edit loop is about 2.4× longer than it needs to be. See E1. |
| Streaming markdown, 20 KB reply in 40-byte deltas **[measured]** | Debug: plain prose 133 ms total (worst frame 0.58 ms). **The same reply with one `` `Vec<T>` `` in its first paragraph: 2.57 s total (19×), worst frame 12.8 ms.** Optimized `local` profile: 29 ms becomes **373 ms (13×)**, worst frame 0.13 ms becomes 1.69 ms. The cost grows with reply length, because the whole tail is re-rendered on every frame. See E2. |
| Harness overhead with an instant scripted provider **[measured]** | About **50 ms of CPU per extra request plus tool round** in debug (wall 5.25 s, CPU 4.95 s, so it's CPU-bound, not waiting on a timer). The `perf` profile: **12.2%** in `Agent::schemas`, of which 11.4% is `contracts::identities` re-hashing every tool schema. A further **~15%** is spent re-serializing the request (`Value::to_string` 8.6%, `wire::ByteCounter` 6.4%). The ~8% in ureq/rustls buffer zero-filling is mostly a debug-build effect and isn't counted. See E3. |
| Scheduling probes (repo's own opt-in tests) | Batching 8 reads takes a turn from about 570 ms to about 220 ms (9 requests down to 2). Moving work to the background halves wall time (413 ms down to 215 ms). Both features work as designed. |
| Test-quality note | The `streaming_render_vec_delta_measurement` probe calls `Theme::dark()`, which parses JSON, on every render. Its result (243 ms against 100 ms for the byte-slice probe) therefore mostly measures theme parsing, not `Vec` deltas. |

---

## 3. Reliability and logic findings

### HIGH: session-breaking or silently wrong results

**R1. One large shell output can stop a session from saving until restart.** **[verified]**
- **Where:** `dial-core/src/runtime.rs:241-247`. `Body::append` caps text only when `kind == "worker"`. Tool output streams in uncapped (`dial-agent/src/agent/ui/state_ui.rs:298`).
- **Mechanism:**
  - On settle, the whole node is snapshotted as one stream frame.
  - A frame over `MAX_FRAME` (32 MB) sets `writer.error` but not `retry_from` (`stream_journal.rs:238-257`).
  - From then on `retry_failed_write` fails at `retry_from.context(...)` (`:302`), and `Session::append` runs `flush_runtime()?` first.
  - So the tool result and everything after it are never journaled.
- **Scenario:** `cat` of a 30 MB log, or verbose test output, where JSON escaping pushes the frame past 32 MB.
- **Fix:** tail-cap tool bodies the way worker bodies are capped (the full output is already spooled to an artifact). Treat an oversized frame as a refusal of that one update, not a writer-wide latch.

**R2. A token refresh or key rotation permanently breaks Responses-protocol sessions, and Gemini too.** **[verified]**
- **Where:** `dial-llm/src/llm/responses.rs:220-228`. `exchange_api` hard-codes `account_id: None`. `native_replay_scope` (`model_config.rs:255-258`) then hashes the bearer token or API key itself. `validate_history` (`responses.rs:784-815`) rejects every stored reply item once the credential changes.
- **Scenario:** xAI OAuth (about 1-hour tokens), or a key pool rotating after a 429. After that, every request fails with "native history belongs to … different account/endpoint; start a clean chat". Gemini (`gemini.rs:852-859`) and Bedrock's STS fallback behave the same way.
- **Fix:** pass the account id on these paths, as the Transport path already does (`provider.rs:2071-2076`). Scope API keys by endpoint or pool, not by key value. On a mismatch, fall back to portable history instead of failing.

**R3. The patch engine can edit the wrong occurrence without any warning.** **[verified mechanism; reviewer reproduced with a Python port]**
- **Where:** `dial-tools/.../edit_native/fuzzy.rs:847-884`. The exact, trim and unicode passes call `sequence_result(…, ambiguous=false)`, so `match_count` is `None` for duplicate exact matches (`:157-170`), and the first match wins. Every ambiguity safeguard downstream (hint preference, hint window, "Found N matches") only fires when `match_count > 1`.
- **Consequences:**
  - Hunk-header line numbers are ignored for exact matches.
  - The context-stripped `TrimCommon` fallback (`patch.rs:1069-1093`) accepts the first exact match of the `-` lines anywhere in the file.
- **Reproduced scenario:** with `fn first(){ return 0; }` and `fn second(){ return 0; }` in one file, a hunk whose context says `second` rewrites `first`, and no warning is produced.
- **Fix:** record the match count in every pass. Prefer the hint among duplicates. Only accept a context-stripped variant when it is unique, or unique within a window around the hint.

**R4. A long command is killed at the 15 s auto-background point when it can't be handed off.** **[verified]**
- **Where:** `shell.rs:810-832` moves the job into `on_adopt`. `adopt_into` fails when `bg` isn't registered (`execution.rs:62-70`, which is the case with `dial --tools bash,read,edit`). It also fails when 8 background jobs are already running (`table.rs:1046`: "…cannot adopt foreground process, which was stopped").
- **What happens:** the job is dropped, its process group is killed, and the captured output is thrown away. Builds and tests die at 15 s despite a 300 s timeout.
- **Fix:** check adoption capacity before handing off, or return the job so it can keep running in the foreground until its own timeout.

**R5. Overlay worker isolation imports the whole build-output tree into a private git repo.** **[verified]**
- **Where:** `isolation.rs:509-515` calls `git::initialize_private_snapshot` (`dial-tools/src/tools/vcs/git.rs:456-470`), which runs `git add --force --all` excluding only `node_modules`, `venv` and `vendor`. The overlay view intentionally leaves `target/`, `build/`, `dist/` and `.cache` visible.
- **Cost:** `target/` in this checkout is **20 GB**. Each isolated worker would compress all of it into a temp-dir repo, which is often tmpfs, so expect minutes of CPU or ENOSPC. The step runs with `NEVER_CANCELLED`.
- **Fix:** add from the snapshot's own path set (`--pathspec-from-file`, no `--force`), or exclude every name in `DEPENDENCY_DIRS`. Make the step cancellable.

**R6. "Auto" isolation never falls back when the overlay mount fails.** **[verified empirically in this container]**
- **Where:** the probe (`overlay.rs:29-40`) checks that overlay appears in `/proc/filesystems` and that a whiteout can be `mknod`ed, but never tries a mount. A later EPERM from `mount(2)` is returned as an error (`isolation.rs:476-483`) instead of moving on to worktree or copy.
- **Observed:** as a non-root user here, five isolation tests failed with exactly "kernel overlay mount failed (Operation not permitted) and fuse-overlayfs is not on PATH". `docs/subagents.md:179` says auto "uses the first one that works here".
- **Fix:** under `Preference::Auto`, try the next backend on any setup error and record why each was skipped. Or make the probe do a trial mount.

**R7. A streamed `context_length_exceeded` is retried as a transient error.** **[verified]**
- **Where:** `responses.rs:672-694`. Unlisted failure codes become `unclassified_failure`, which is retryable and `Transient` (`:16-37`). `classify` returns before the text patterns could catch it.
- **Effect:** the same oversized prompt is resent `max_attempts` times with the 300 s waits from part 1, instead of compacting. `usage_not_included` is handled the same way.
- **Fix:** map that code to ContextOverflow and quota codes to QuotaExhausted, and keep the raw code for the text classifier.

**R8. A torn final line, once "healed", is reported as corruption on every later load.** **[verified mechanism]**
- **Where:** `session.rs:1258-1276, 1380, 2814-2818`. An unterminated last line is tolerated as `TornTail`, and `pending_newline` is set. The next append writes `\n` first, which turns the torn bytes into a terminated malformed line. Every later load classifies it as `MalformedInterior`.
- **Effect:**
  - Every resume injects "resumed history may be incomplete" into the model context.
  - `open_or_fork` refuses to fork for good (`:1151-1162`).
- **Scenario:** one ENOSPC or kill during an append.
- **Fix:** record the sealed offset as a marker, or truncate the torn bytes while holding the writer lock.

**R9. Workers that stopped cleanly can never be restored, and a settings rebuild silently kills background workers.** **[verified status mapping]**
- **Where:** `live_tasks.rs:1324-1325` persists every `Phase::Interrupted` as `"cancelled"`. `app/restore.rs:32-37` restores only workers whose status is `"interrupted"` or missing.
- **Effect:**
  - Restore works only after a crash.
  - Quitting, or a `Step::Rebuild` (for example `/model`), drops `LiveTasks`, which cancels and joins every worker.
  - Results that were never delivered vanish.
- **Fix:** persist owner shutdown and rebuild as `interrupted` (distinct from a user cancel). Carry `LiveTasks` across rebuilds, or defer the rebuild while workers run.

**R10. One-shot notes are lost whenever a reply is discarded after it arrives.** **[verified]**
- **Where:** `run/turn/step.rs:946` calls `turn.reply_received()` on every `Ok(reply)`. That clears `pending_nudges` (`run/turn.rs:145-151`) before the truncation raise, rejected edit preview, constrained repair, guidance regeneration and research retry paths throw the reply away.
- **Contradiction:** the method's own doc comment says discarded requests resend the notes.
- **Effect:** background "job finished" notices, monitor deliveries, hook context and the one-time 80% wrap-up notice can disappear.
- **Fix:** call `reply_received()` only once a reply is kept, or re-queue the notes on each discard path.

### MEDIUM

| # | Finding | Location | Fix |
|---|---|---|---|
| R11 | Goal mode: parallel read batches and streamed-reuse calls never reach `Lifecycle::dispatched`, so a continuation turn that only batched reads counts as stalled and goal automation stops early. | `run/turn/round.rs:557-617` vs `modes/goal.rs:373,417` | Call `dispatched` for every executed result, next to `settled`. |
| R12 | The prewalk / plan-yolo model handoff swaps the backend without `install_route`. Context window, compaction thresholds, cache namespace and `max_tokens` stay on the planner's values, so a 32k model gets 200k-sized thresholds and the turn goes into overflow recovery. | `modes/prewalk.rs:246,259,345-347`; `model/route.rs:69-87` | Route the handoff through `install_route`, then refresh `turn.max_tokens`. |
| R13 | The "call rewind" reminder loops without limit. A text reply during an active investigation checkpoint pushes a reminder and returns `Again`, with no counter, no setback charged, and `max_steps = 0` meaning unlimited. Each loop is a paid request. **[verified]** | `run/turn/step.rs:1548-1551` | Charge a setback or cap it per turn, then stop as Incomplete. |
| R14 | Anthropic replays stored native content (thinking signatures) to *any* Anthropic-protocol route: zai, minimax, vercel, cloudflare. After a fallback or revert, foreign signatures go to Claude. Thinking is also enabled even when the last tool turn had none. | `dial-llm/src/llm/provider.rs:2665-2699,2884-2897,3180` | Stamp scope and model on the blocks; fall back to portable form on a mismatch. |
| R15 | The Claude subscription token is fetched once and reused across all retries (each up to 300 s), so a retry run that crosses expiry gets a 401 with a misleading "check ANTHROPIC_API_KEY". | `provider.rs:1500-1505,2151-2163` | Re-fetch each attempt; refresh once on the first 401. |
| R16 | Chat-completions finish reasons aren't normalized. `model_context_window_exceeded`, `network_error` and Mistral's `model_length`/`error` are accepted as complete answers. | `stream.rs:584-594,708`; `reply_triage.rs:51` | Map them to `length` or `IncompleteStream`, as the Anthropic and Bedrock paths do. |
| R17 | Output-limit truncation of a tool call is Fatal on Gemini and Responses, which bypasses the automatic budget raise that the other paths get. One Anthropic overflow message ("input length and `max_tokens` exceed context limit") matches no overflow pattern, so it is classified Fatal. | `gemini.rs:731-735`; `responses.rs:663-671,719-731`; `errors.rs:325-358` | Return `length`; add the pattern. |
| R18 | Gemini rejects tool history from any other model or route, so fallback or promotion in a tool-using session fails, defeating `model-routing.md`. | `gemini.rs:249-275` | Use Google's documented placeholder `thoughtSignature` for foreign history. |
| R19 | Patch re-indentation replaces any new line that matches a unique old line with the old line *including its indentation*, so a deliberate dedent is silently undone (a semantic change in Python or YAML). Reproduced by the reviewer. | `edit_native/patch.rs:242-271` | Apply a uniform indentation delta, as `text.rs::adjust_indentation` does. |
| R20 | `grep` and `glob` scoped to a subdirectory ignore ancestor `.gitignore` files, so `dist/`, `.next/` and `coverage/` use up the 50k-entry and 60 MiB page budgets before real source is reached. | `fs/walk.rs:238,368`; `fs/search.rs:587,1702` | Load the `.gitignore` files between root and scope. |
| R21 | One broad grep page records witnesses for about 300 files against a 256-file store, which evicts the read and edit witnesses the model is working with. The next edit then fails with "no read of this file is recorded". | `fs/search.rs:1525`; `edit/hashline.rs:47-49,308-321` | Give search witnesses a separate budget or lower priority. |
| R22 | Worker edits under `build/`, `dist/`, `vendor/`, `target/` or `.cache` at any depth are dropped from merges while the merge reports `integrated: true`. Permission-only changes (`chmod +x`) never merge on the copy, clone or worktree backends. | `workspace_copy.rs:198-208`; `isolation.rs:45-59,216-222,295-297` | Exempt tracked paths; add mode and ctime to `Identity`. |
| R23 | The worktree backend gives workers no dependencies (`node_modules`, `.venv` and `vendor` are skipped and the dependency copy isn't run). Acceptance revalidation compares the parent and copy with different exclusion sets, so a stale `__pycache__` rolls back good patches. | `isolation.rs:499-507,748-751`; `worktree.rs:110-193`; `verify/inputs.rs:189-216` | Run the dependency copy for worktree; capture both sides with `capture_excluding`. |
| R24 | What a rewind shows depends on whether stream reclamation has run (acknowledgements come from all branches, restoration from the active branch only). Stream-recovered nodes are marked persisted and never reach the main journal. | `session.rs:3122-3188`; `runtime.rs:429` | Use one branch rule; mark stream-newer nodes dirty; flush before leaf moves. |
| R25 | Checkpoint diff (`TextDiff::from_lines`) has no deadline on files up to 8 MB, and restore runs it twice per file while holding the checkpoint lock, which blocks every tool's `before_write`. | `storage/checkpoint.rs:304`; `review.rs:261,300` | `TextDiff::configure().timeout(..)`, as the edit preview already does. |
| R26 | Ctrl+O and Ctrl+L commit the whole transcript as one terminal frame. Over 16 MiB it is refused, every later paint fails, and the session ends. `leave()` puts its mode resets (bracketed paste, keyboard flags) in that same all-or-nothing frame, so the user's shell stays in a broken mode. | `app/shell/input.rs:257`; `ui/term.rs:403-407,756-784`; `term/output.rs:233-238` | Replay through the budgeted retirement path; send resets as a separate small frame that is always admitted. |
| R27 | Idle cache warming loses money. Each 4-minute replay is judged on its own, so beyond about 46 minutes the replays (about 1.4× input) cost more than the miss they prevent, even if the user returns. | `dial-llm/src/llm/cache_warmer.rs:82-99` | Compare total replay spend against the expected miss cost, or cap the number of replays. |

### LOW

**Edit and tool output**
- `\r\r\n` line endings lose a CR on lines the edit never touched (reproduced) (`text.rs:75`).
- Bash capture cuts the head at exactly 6144 bytes, so a multi-byte character straddling the cut becomes "��" even when nothing was dropped (`shell.rs:200-237`).
- Sloppy mode silently resolves an ambiguous operation by exclusion, and drops overlapping edits from the same operation (`sloppy/apply.rs:1239-1252,2457-2481,2847`).
- With `format_on_write`, the recorded witness and the reported tag describe the text *before* formatting, so the next edit is refused (`lsp/mod.rs:114-123`; `editing.rs:1348-1391`).

**Workers and turns**
- A worker whose patch merged is reported `failed` if writing its report fails afterwards (`live_tasks.rs:1383-1401`).
- In dependency-wave dispatch, the specific pre-edit review text is generated and then discarded (`round.rs:476-477`).

**Providers and sync**
- Tool-call deltas are merged by `index` only (`stream.rs:609-680`).
- LSP `sync_document` holds the docs lock across a send that can wait 10 s, and a timed-out acknowledgement reuses the version number (`client.rs:894-946`).

**Durability**
- `set_additional_dirs` renames over the session file without fsyncing the directory.
- `create_file_at` never fsyncs.

**Terminal and UI**
- `restore_inline_screen` clears `IN_ALT_SCREEN` even when its write failed.
- The panic hook can block on the stdout lock, which defeats the 500 ms `PANIC_FLUSH` bound (`term.rs:722-728,948-950`).
- An unbracketed multi-line paste on Windows can submit at the first Enter (`ui/input.rs:82-98`).

---

## 4. Efficiency findings

### E1. The incremental build rebuilds crates that didn't change **[measured]**
`crates/dial-build-info/build.rs` reruns on any change under `../../crates` and embeds `SystemTime::now()` as `DIAL_BUILD_EPOCH`. Every crate that depends on it then sees changed env input and recompiles. Yet `dial-tools` uses only `build_info::VERSION` (`dial-tools/src/lib.rs:8`, `browser_relay/server.rs:219`, `report_tool_issue.rs:12`), and `dial-agent` uses it only for `/api/version`.

- **Measured:** touching one `dial-ui` file takes 27.4 s, of which about 16 s is avoidable.
- **Fix:**
  - Read the volatile values (revision, dirty, epoch) only in the root binary, for example with a tiny `dial-build-stamp` crate that only `dial` depends on.
  - Have library crates use `env!("CARGO_PKG_VERSION")`.
  - For the epoch, use the commit time (`git log -1 --format=%ct`) rather than `now()`. That also fixes the reproducibility issue from part 1.

### E2. Incremental markdown streaming switches itself off for most coding replies **[verified + measured]**
`dial-ui/src/ui/transcript.rs:446-449`: if the uncut tail contains `[` or `<` anywhere, the whole remainder is re-rendered from scratch. The tail only grows, so the cut never advances again. That covers `Vec<T>`, `a[i]`, links, `x < y`, generics and JSX, which is nearly every coding reply.

- **Measured** on a 20 KB reply: 19× more total CPU and 22× worse worst-case frame in debug; 13× in the optimized profile (29 ms vs 373 ms). The measurement used a temporary copy of the repo's own `streaming_render_measurement` probe, which was reverted.
- **Related costs:**
  - `join_rows` deep-copies the frozen prefix on every frame (`:478-490`).
  - `spill_point` re-renders `text[..cut]` from byte 0, which is O(n²) over a reply (`:402-409`).
  - `invalidate_tail` forces the live answer to re-render while a long `write` streams its arguments (`:902-906`).
- **Fix:** `markdown::render_split` already computes a parser-checked stable cut that `markdown.rs:30` throws away. Use it instead of the character heuristic. Keep rows in chunks instead of copying the prefix.

### E3. Every request reprocesses the whole history several times, so cost is quadratic over a session **[partly measured]**
Each item below is cheap for a short session but grows with history length, and it runs on every request, round or call:

| Work | Path | Location |
|---|---|---|
| All tool schemas rebuilt from scratch and identity-hashed. The result is thrown away (`Catalog::from_schemas` is called only as validation). This is about 12% of CPU in the profile above. | ≥2× per round | `run/tool_exposure.rs:532-647`; `tools/surface/contracts.rs:731-743` |
| The serialized request is re-parsed, re-serialized and hashed for `wire::record`. A `json!` deep copy of messages. Serialize → parse → serialize on the Claude OAuth attestation path. About 15% of CPU in the profile. | per request | `dial-llm/src/llm/wire.rs:47-119`; `mod.rs:887-896`; `provider.rs:2496,3061-3082` |
| 5–6 whole-request passes: raw estimate, evidence-suffix estimate (keys embed the full tool-output text), accounting, prefix digest (to_vec + SHA-256 per message), contract projection per assistant message, and an eager `fixed_tokens` clone. OpenAI exact-id counting deep-clones every message twice. | per request | `window/projection.rs:32`; `workflow/evidence.rs:105-110,293-306`; `window/request_prefix.rs:69-79`; `counting.rs:98-110`; `step.rs:243,352` |
| Full deep clone of `session.messages()` before an early return (every `read`), and in `offered_with` when `snapcompact.overlay` is off, which is the default. | per call / per round | `run/dispatch.rs:1003,1625`; `run/tool_exposure.rs:446,541` |
| Every historical image is re-read and SHA-256'd. Images over 2048 px or 5 MiB are **decoded, Lanczos3-resized and re-encoded on every request.** | per request | `dial-llm/src/llm/images.rs:247-266,379-491` |
| `model-catalog.json` is re-read and re-keyed on every role or backend build. | per backend build | `catalog.rs:2461-2495` |
| Stream events are parsed 2–3 times each (Anthropic 3×, Responses 2×, Codex WS 1 parse + 2 `to_string` + 2 re-parses). | per event | `anthropic.rs:168,387`; `provider.rs:2571`; `responses.rs:541,1114`; `codex_ws.rs:674-723` |

**Reviewer estimate:** about 10–25 ms per request at 200k tokens, and about 5× that at a 1M window, before the network. The common fix is to cache per-message derived values (wire parts, digests, token counts) by entry index, since stored messages are immutable. Memoize schemas and identities keyed by (offered names, edit mode, config revision, registry revision). Cache resized images by (sha256, edge, detail).

### E4. Background and idle work that never stops

| Finding | Cost | Location |
|---|---|---|
| `/btw` streaming re-serializes all side threads and runs `write_private` (fsync, rename, directory fsync) **on every UI refresh tick with new text**, on the UI thread. | Estimated 10–40 ms per save on macOS, 12–25 times a second | `dial-app/src/app/side.rs:343-344`; `platform/atomic.rs:97-117` |
| The project file list is rewalked every 30 s forever (up to 50k entries / 2 s), and each listing restarts `scan_vocabulary` (reads up to 32 MB of source) even when word completion is off. The "File completion is partial" notice repeats every 30 s. | Disk and CPU every 30 s while idle | `app/shell/file_discovery.rs:12,80-96`; `ui/composer.rs:660,1291-1305`; `word_complete.rs:291-327`; `lifecycle.rs:198-200` |
| The RPC state projection rebuilds `execution_tree()` (two full copies of every message plus all runtime node text) up to 10 times a second whenever a worker streams, while holding the cache mutex. | Estimated 10–20 MB copied per rebuild on a 4 MB session | `dial-app/src/rpc.rs:445-446,785-791`; `rpc/actor.rs:235-243`; `session.rs:3027-3067` |
| Every paint locks and snapshots *every* worker node the session ever had, and the tree is never pruned. `elapsed_ms` at millisecond resolution means the task card re-renders every frame. The worker viewer's cache key includes the spinner tick, so the whole worker transcript is rebuilt about 12 times a second. | per frame | `app/shell/paint.rs:212-218`; `transcript.rs:1569-1611`; `overlay/subagent_viewer.rs:739-740` |
| Running bodies are re-journaled in full on every append: about 20 MB of journal for 4 workers over 300 appends, all re-parsed on load. Reclamation can rerun with three fsyncs on every flush once retained data passes 4 MB. | Journal size and load time | `session.rs:3087`; `stream_journal.rs:394` |
| Background reader threads poll every 10 ms even when idle. | About 100 wakeups per second per job | `exec/background/table.rs:233-235` |

### E5. Tool algorithms

| Finding | Cost | Location |
|---|---|---|
| Fuzzy matching runs a full Levenshtein per window, O(lines × hunk), with no early exit, normalization redone per window, and 10–30 full passes per missed hunk across the fallbacks. It never checks for cancellation. | Estimated minutes for one failed hunk on a 4 MiB file | `edit_native/fuzzy.rs:602-656`; `patch.rs:525-558,1069-1093` |
| The `find` evidence cache is a 128-entry LRU swept sequentially, so on any repo with more than 128 files the hit rate is 0% and every call re-tokenizes up to 64 MiB. | per `find` call | `code/navigation/cache.rs:9,206-238`; `navigation.rs:559` |
| Parallel grep creates a new `thread::scope` with up to 4 threads for every 4 files. | About 50k thread spawns per 50k-file page; the docs say "up to 8 workers" | `fs/search.rs:209-240,910-930` |
| Worktree setup lists every *file* inside ignored directories (`--ignored=traditional --untracked-files=all`), then discards them all. That is all of `target/` and `node_modules`. | Hundreds of thousands of entries per worker | `workers/worktree.rs:117-126`; `workspace_copy.rs:210-235` |
| One paint per input event, plus a whole-draft layout with one `String` per character. | O(N²) for an unbracketed paste (Windows console input) | `app/shell/run.rs:369-393`; `ui/composer.rs:2682-2696` |

---

## 5. Checked and sound

- **Turn loop:**
  - Every call id is answered exactly once and in order, including skipped, steered, interrupted and duplicate calls. Parallel results pop in order.
  - The no-reply watchdog (24 steps), setback ledger (16), overflow retries (3 per reply) and compaction re-entry guard all hold.
  - Repair and stop-hook rounds share one turn budget.
  - The prompt-cache prefix is deterministic (BTreeMaps, sorted names, memoized evidence placeholders).
  - Tool panics are contained.
- **Providers:**
  - Usage is counted once across retries.
  - Interrupted or half-read connections are never returned to the pool.
  - Idle timeouts aren't reset by keep-alives.
  - Account and key rotation can't spin between exhausted credentials.
  - tiktoken is a singleton with a digest cache.
  - `json_repair` refuses cut-off arguments.
  - Streaming healers are bounded.
- **Storage:**
  - Dirty-node acknowledgement compares full node content.
  - Recovery interrupts every Running body.
  - A main-journal write failure drops writer ownership, so nothing is ever appended onto a partial line.
  - No fsync per stream frame.
  - Delta frames require an exact revision.
  - Branch rebuild is linear with a cycle guard.
  - Migration is idempotent and refuses newer schemas.
- **Workers:**
  - The merge transaction checks every path before writing and rolls back only files still equal to the worker's version.
  - Every job settles exactly once.
  - Approval and question waits can't hang.
  - Resume validation holds.
- **Tools:**
  - Edit transactions capture everything before installing and roll back in reverse with an ownership check.
  - Hashline applies against one snapshot in a single pass.
  - Replace mode requires a unique or dominant match.
  - BOM, CRLF and final newline round-trip for normal files.
  - Grep compiles its regex once and prunes ignored directories.
  - Bash kills the whole group, with grace for grandchildren.
  - LSP framing is bounded and pending requests are cleaned up.
- **UI:**
  - The output queue refuses frames rather than dropping control sequences, and commits terminal state only on acceptance.
  - Identical frames are skipped.
  - The retirement budget holds.
  - Size caps exist for tool arguments and output in the transcript.
  - Long lines are linear.
  - RPC deltas are cursor-based.
  - Idle frame pacing is 1–2 s.

---

## 6. Suggested order of work

1. **Session-breaking bugs, each a small local change:** R1 (cap tool bodies, refuse oversized frames per update), R2 (stable replay scope), R7 and R16 (classification tables), R8 (heal torn tail), R4 (don't kill on failed adoption).
2. **Wrong-result bugs:** R3 and R19 (patch ambiguity and indentation), R10 (re-queue notes), R13 (bound the rewind reminder), R11 and R12 (goal activity and route install).
3. **Worker isolation:** R5 and R6 together (snapshot only the snapshot's paths; auto fallback), then R9, R22 and R23.
4. **Cheap, large efficiency wins:**
   - E1 (build-info split, about 2.4× faster edit loop).
   - E2 (use the parser's stable cut, about 13× on code replies in an optimized build).
   - `/btw` and 30 s idle rescans (E4).
   - Memoized schemas and identities (E3, about 12% of turn CPU).
5. **Structural:** cache per-message derived data by entry index (E3). That turns the quadratic per-session cost back into linear, and it is the single change with the largest long-session impact.

To keep these fixed, add regression tests for each (the probes in `ui/transcript/tests.rs` and `tests/integration/agent/scheduling_measurements.rs` are a good template). Also add a CI-recorded (not gated) per-request overhead probe at 50, 200 and 500 messages, so the E3 scaling shows up in numbers.
