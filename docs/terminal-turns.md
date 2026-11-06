# Turns, steering and review

Steer running work, queue follow-ups and review recorded outcomes without confusing history with fresh verification.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

## Terminal shutdown and repository reads

On Unix, SIGTERM, SIGHUP and SIGQUIT request normal UI unwinding so terminal modes and owned
relay children can be cleaned up. Handlers only record the request; input waits observe it at
bounded polling boundaries. Ctrl-C retains its turn-interruption behavior. Non-cancellable
external work can still delay shutdown, and SIGKILL cannot run cleanup. Startup clears the
visible screen without erasing the user's scrollback.

Returning from a full-screen panel redraws inline rows without a display-wide erase. This keeps
streamed answers from appearing twice in scrollback under tmux's `scroll-on-clear` behavior;
no tmux configuration change is needed. Normal redraw, terminal exit and external-editor handoff
use non-scrolling row clearing. Explicit display/history reset remains a separate action.

The Git overlay loads status and diffs on bounded, coalescing background workers. Selection
generations reject stale replies. While repository reads are pending, mutations are refused;
an unavailable diff never turns a hunk-stage shortcut into whole-file staging.

## Recovering from a full disk

If a turn stops with `stream journal failed: … No space left on device`, free space on
the volume holding Dial's session data, then send `continue`. On the next turn Dial
retries the failed stream writer without restarting. It removes only the failed append's
possibly incomplete row; previously committed observations remain intact. The failed
operation's unpublished output is not recovered, and tools are not automatically replayed.
If storage still cannot be written, the turn stops again with the write error. Removed,
replaced or externally truncated journals are refused rather than recreated; non-write
failures (including checkpoint failures) are not reset by this recovery.

- **Work plan:** the live summary shows the current step, blockers and the next step rather than
  repeating the whole checklist. `/todo` opens a searchable inspector, ordered by current work,
  blockers, upcoming steps and closed history. Select a step for its full recorded reason,
  finding and evidence; Back returns to the plan. Browsing never changes task state.
  `/todo list` retains plain-text output and appends the progress view; `/todo show <number>` opens
  a recorded step directly. `/todo progress` shows persisted deliverables with their claimed status,
  explicit linked-check verdicts (including stale or unknown evidence), configured acceptance,
  unverified references and worker acceptance. Opening it while idle revalidates existing input
  witnesses without running checks. Unrecorded ownership stays unknown; passing checks do not
  imply complete semantic coverage. While a turn runs, the new progress section reports unavailable
  instead of presenting an old snapshot as current; the existing todo browser remains available.
  Todo findings and references are labelled as agent observations, not verification.

- While a turn runs, submitting a non-command draft opens **a message-intent chooser**. Nothing is
  sent just by opening it; Escape returns to the unchanged draft. The options are **Steer** (by default
  steers this task with no model call; `[message_routing] route = "classify"` lets a routing judge decide
  between steering and an independent worker, and a failed decision steers) and **After this turn**
  (queued, unchanged). Two advanced keys are shown in the chooser footer:
  `r` (or `Ctrl+Enter` where the terminal reports it) is a hard redirect: it cancels the pending model
  request or discards its stale response before any of its tool calls run. A tool already running may
  finish (only waits that are safe to interrupt are interrupted); unstarted actions are skipped and
  completed results are retained. The latest instruction takes precedence over conflicting older plans.
  `w` (or `Ctrl+W`), like `/worker <message>`, starts a separate worker for exactly that message right
  away (even mid-reply or while tools run) while the current task continues; it never steers the running
  task and no model classifies it. If a worker cannot start (spawning disabled, capacity, interruption)
  the message is queued as a follow-up instead. With no turn running, `/worker` says so and sends the
  message as a normal task.
  Explicit messages are shown above the composer, one row each (`○ Steer  pending · acts at the
  next safe point  “message”`, with ● received and ✓ applied), with the stage Dial actually reported:
  *pending* (typed, not yet read by the agent; it acts at the next safe point), *received*
  (the agent took it and is cancelling the stale reply) and *applied* (recorded as the current
  instruction, or the worker started). If the turn ends before a message was applied it is queued
  to send next rather than lost. They are not part of `/queue`.
  `Ctrl+Q`, or `Option+Enter` on macOS (`Alt+Enter` elsewhere), still queues a follow-up directly.
  The queue above the composer previews up to three messages (`Queued  send after turn  “message”`)
  and counts the rest.
  Empty Enter with queued work opens a separate choice, defaulting to **Keep working**;
  stopping to send the oldest message requires explicitly selecting that option and confirming.
  `Up` edits the last queued message (`/queue` edits any item). Saving an edit preserves its queue
  position and route; Esc restores the original. Interrupting preserves the draft and queue.
  The status line reports stopping immediately; interrupted turns do not send a finished notification.
  Slash commands never become model steering: `/hotkeys`, `/todo`, `/skills`, `/commands`, `/context`, `/search`, `/workers`, `/message`,
  `/cancel`, `/policy`, `/reliability`, and `/improve status|jobs|stop` run at once without stopping the turn.
  `/context` also lists the last request's estimated tokens by category: system prompt, context files, tool definitions,
  user and assistant messages, one row per tool's output, images at a fixed estimate, and provider replay shown by size only.
  These are byte-based estimates, not billed usage; during a running turn it shows only the summary line.
  `/help` opens the action palette while idle or busy; `/help text` and `/help all` list commands and their scheduling policies,
  also used by dispatch and palette notes. `/model` and `/models` share the same policy, including
  `settings` (`setting`), `defaults`, `local` and `memory`: these panels open immediately, even during
  a turn. The model shortcut (`Ctrl+K` on macOS, `Alt+M` elsewhere) opens the same picker.
  Saved choices apply after the current turn without canceling it; display settings apply immediately.
  Closing the picker without selecting changes nothing. A selection changes only this session and the
  saved default for new sessions; other running Dial sessions keep their models.
  Other commands wait for the turn to finish, with a notice, except commands
  that replace or alter the active session/work: `/exit` (`/quit`, `/q`), `/new`, `/clear`, `/resume`, `/sessions`,
  `/tree`, `/fork`, `/undo`, `/rollback`, `/compact` and `/plan` stop it first.
  `/queue` inspection and queue edits also leave the current turn running. `/model restart` (also
  `/models restart`) is refused
  while busy without cancelling inference. Live informational notices do not settle or replay the
  streaming answer.
  The shortcut editor (`/hotkeys`) also stays fully editable during a turn. Provider connection tests
  return to the provider list while discovery runs, rather than silently closing the panel.
  During compute waits and local-model preparation, provider/model/settings navigation, tool and
  reasoning visibility, tool expansion, screen reset and dictation remain available. Escape only
  dismisses a local completion/search/send chooser; `Ctrl+C` (or `/cancel`) cancels the wait. Screen
  reset works immediately during inference instead of waiting for the turn to finish.

- A turn that edited files, was interrupted or failed ends with a compact **Turn report** line (for
  example `3 files changed · checks passed · 1 open task`) and a muted next-step hint
  (`/review · /undo` after edits, `continue · /undo` after a failure). The collapsed check state is
  `checks failed`, `checks stale` (recorded before the final edits), `checks unverified`, `checks passed`
  or `no recorded checks` (checks are optional; a shell or `eval` check is not listed here).
  The tool-expand key (`Ctrl+O`) shows the full report: outcome, changed files, recorded checks, where
  their logs can be opened with `/evidence check`, open tasks, shell or extension calls outside `/undo`,
  and request timing. Empty sections are omitted, and plain answers with no edits show no report. The
  summary is stored in session control state as `turn_summary`.
- Settled test and build commands (`cargo test`, `cargo clippy`, `make test-*`, `pytest`, `tsc`, …)
  show a **Test** or **Build** card instead of their raw log: counts at the right of the header, then
  only failing test names or the first errors with their `file:line` locations. The expand key
  (`Ctrl+O`) shows the ordinary card with the complete output. Output the card cannot attribute (a
  crash with no test or error lines, a job moved to the background) keeps the ordinary card.
- Resuming displays durable todos and recorded check statuses; these are history, not fresh test
  results. Completed tools are not replayed automatically. `/review` reviews a frozen local or GitHub
  PR scope with source-backed findings (the captured diff is split on `diff --git` file boundaries into
  chunks of at most 20,000 bytes, byte-splitting only a single larger file; chunks run concurrently as
  read-only `reviewer` tasks up to `task.max_concurrency`, and an interrupt cancels all of them. Findings are
  merged, exact duplicates (path, range, title) dropped, and sorted by priority then confidence. The report
  shows target identity, an unchanged/stale line, a verdict (`do not ship` for any P0/P1; else `inconclusive`
  if any chunk failed or was inconclusive; else `ship with fixes` for P2/P3 only; else `ship`), counts per
  priority, and findings grouped P0..P3, followed by the same data as machine-readable JSON); `/review-file` lists checkpoint turns, then groups the turn's changes by writer
  (`main` or a worker label; checkpoints from before attribution show as `unattributed`).
  A group offers **Review files** and **Revert unshared files**. Unshared files were changed only
  by that writer and in no later checkpointed turn. Group revert first shows the combined diff
  (including your own later edits; above 24 KiB it is refused and files must be reverted one at a
  time). Each file is compared with its reviewed version right before it is written. This is not a
  transaction: if one file fails or changed, the revert stops, files it already wrote are put back
  only if they still hold exactly what it wrote, and anything else is left as is with the revert
  checkpoint kept for `/undo files` (which would overwrite those files, so review them first). One
  file-only checkpoint covers the revert. A file written by several writers, or changed again in a
  later turn, is marked *overlap*: that is a risk signal, not proof either way, so it is never part
  of a group revert and a single-file revert asks for separate confirmation. No recorded overlap
  does not prove a file is
  independent of other code (a renamed function and its callers). `/review-file <turn> [path]`
  previews one file with **Keep / Revert file / Cancel**. The review also says how many shell
  commands and extension calls in the latest turn were not checkpointed: their effects cannot be
  shown or reverted here. Files too large to checkpoint are listed as not reviewable.
  A revert includes all changes shown for that file, including your own edits: review carefully.
  Changes after preview are refused, other files are untouched, and the revert has its own
  `/undo files` checkpoint. Bare `/undo` also rewinds conversation and refuses unrelated newer file
  checkpoints, including a review revert. Private, binary or oversized interactive diffs require manual review.
  The checkpoint lock only keeps other checkpoint recording out; edit tools and external programs
  are not excluded, so a change can still land between a file's compare and its write. This is not
  filesystem-atomic: avoid editing the selected files concurrently with a revert.
- Request status distinguishes waiting, model loading, prompt processing, generation, retry and
  cancellation. Time without a worker progress event is separate from total task elapsed time.
  `/model restart` explicitly confirms unloading only Dial's managed local main model, which reloads
  with the same selection next request. Busy turns, running children and external endpoints are
  refused; the command never switches models or kills external servers. Failed/cancelled inference
  on a Dial-managed server retires its process before retry so abandoned work cannot block the next request.
- `/plan [on|off]` enforces read-only planning. The model's `exit_plan` request asks for approval
  before enabling edits. `/permissions` lists scoped rules; for example,
  `/permissions allow --persist bash cargo test` persists a project rule outside the workspace.
  Deny rules take precedence, including in automatic approval mode.

## Workspace rail and live worker cards

The optional **workspace rail** above the composer shows per-file additions/deletions, including
pre-existing changes; new files are labelled `new` and binary diffs `binary`. It starts off.
`/changes` lists the cached workspace snapshot.
**Pinned workers** sit above the composer and show running and recently finished workers: a header with the active and done counts, up to three rows in collapsed mode, a live line with the latest output (when `ui.worker_live_preview` is on) and an elapsed clock after five seconds. `ui.pinned_workers` selects `collapsed` (default), `full` (more rows) or `off`. The block is hidden on terminals shorter than 16 rows. The `workers` status segment shows `running/total workers`, adding `· $cost` once a worker reports cost; the block, the segment and the hub read the same snapshot. When a worker finishes, needs input or fails while no subagent view is open, one short notice names it and points to `Alt+A`. `Alt+A` opens the subagent hub. `Alt+Shift+A` opens the transcript of the most recently active running worker; press it again to cycle through the running workers. With none running it cycles through all workers, and with none it opens the hub. Double-Left on an empty composer also opens the hub. Pinned rows are not clickable: mouse capture is enabled only while a full-screen panel is open. Rebind `app.subagents.recent` to change the transcript key.
Toggle the block in `/settings` → Appearance → Display. Ctrl-P and the empty slash menu start with everyday
actions; advanced controls remain searchable. `/workers`, `/processes` and `/compute` group work
controls. The evidence-expansion shortcut (Ctrl+O by default; rebind it in `/hotkeys`) expands
earlier reads, searches, checks and worker reports. Card hints show the current binding.
Reasoning starts collapsed and per-turn statistics start hidden. Cache accounting and routine
decision advice stay out of the transcript. Viewed images use compact inline previews when supported
by the terminal; optional capabilities remain available through `/setup` without a startup dialog.

## Small terminals and safe confirmation

Home keeps the selected session or starter in view, even when there are only a few rows. Starters
load into the draft for editing; they are not sent automatically. Approval policy and sandbox
isolation have separate rows so the isolation setting does not disappear behind a long mode label.
With automatic approval and no sandbox, the home explicitly warns that tools are not isolated.

While Dial works, an empty composer explains that typing adds direction. Queued-message hints
link to `/queue` and distinguish sending a new direction from stopping to send queued work.
Search panels distinguish an empty collection from no matches and offer a way back. In sessions,
`Esc` clears search before closing; actions on the selected journal still respect ownership and
the existing delete-confirmation flow.

Approval and recovery choices pause confirmation below 36 columns or 11 terminal rows. Resize
to reveal the details and choices; cancellation remains available. `Enter` and approval hotkeys
cannot authorize an option hidden by the resize message. Above that minimum, page keys scroll
long details and navigation keeps the current choice visible. Question dialogs likewise window
their current choice rather than truncating it out of view.

See the [visual review sheet](tui-rendering.md#visual-review-sheet) for the terminal cell gallery.
