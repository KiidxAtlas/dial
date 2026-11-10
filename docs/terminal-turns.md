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

- While a turn runs, submitting a non-command draft opens **a send-action chooser** by default.
  Nothing is sent merely by opening it; Escape preserves the draft. Arrow keys and Enter select, or
  press digits `1`–`4` to pick an option directly:
  **Let Dial choose (auto)** (uses configured routing; may redirect or request a worker),
  **Redirect current task now** (explicit steering at the next safe point), **After this turn**
  (queued follow-up that leaves the current task unchanged), or **Run independently** (requests a
  separate worker and falls back to an after-turn follow-up if it cannot start). The footer states
  what Enter will do. In `/settings` → Input, **Enter while working** explicitly saves `ui.busy_send`:
  `choose` (default), `redirect`, `after_turn`, `independent`, or `auto`. Choices other than `choose`
  send directly on Enter; selecting a chooser action never changes this preference. Empty Enter
  with queued work still asks before stopping, regardless of preference. Automatic routing defaults
  to steering without an extra model call; `[message_routing] route = "classify"` opts into a routing
  judge, and a failed decision steers. `Ctrl+Enter` redirects directly when the terminal supports the
  kitty keyboard protocol. Plain letters and editing chords such as `Ctrl+W` close the chooser and
  reach the draft, so typing is never swallowed.
  Redirect cancels the pending model request or discards its stale response before its tool calls
  run; an already running tool may finish. Completed results are retained and unstarted actions
  skipped. `/worker <message>` uses the same independent route; while idle it submits a normal task.
  Explicit messages are shown above the composer, one row each, with the controller stage:
  *pending* (awaiting a safe point), *received* (dequeued by the controller, awaiting a safe-point
  decision), and *recorded* (now the current instruction, or the requested worker started).
  Receipt alone does not prove cancellation, provider receipt, or model compliance. Runtime
  cancellation/discard events report their own observed effects. Batched redirects retain submission
  order. If the turn ends before a message was recorded it is queued to send next rather than lost.
  Steering rows are not part of `/queue`.
  `Ctrl+Q`, or `Option+Enter` on macOS (`Alt+Enter` elsewhere), still queues a follow-up directly.
  The queue above the composer previews up to three messages (`Queued  send after turn  “message”`)
  and counts the rest.
  Empty Enter with queued work opens a separate choice, defaulting to **Keep working**;
  stopping to send the oldest message requires explicitly selecting that option and confirming.
  `Up` steps through queued messages before prompt history and edits the selected one (`/queue`
  edits any item). Saving an edit preserves its queue
  position and route; Esc restores the original. Interrupting preserves the draft and queue.
  While a turn runs with a draft in the composer, the first `Ctrl+C` clears the draft (the hint
  reads "Ctrl+C again stops") and the next stops the turn. In a panel opened during a turn, the first
  `Ctrl+C` only closes the panel.
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
  dismisses a local completion/search/send chooser; `Ctrl+C` (or `/cancel`) cancels the wait and
  returns your message to the composer. Screen
  reset works immediately during inference instead of waiting for the turn to finish.

- Edited, blocked, interrupted and failed turns end with an **actionable completion footer** from
  the existing recorded turn report: changed files, named checks and their freshness/verdicts,
  outstanding obligations, effects outside `/undo`, and a safe next action. Check freshness alone
  never means passing. Missing freshness/verdicts remain unverified; recorded shell tests are not
  silently promoted to configured checks. Detail is bounded; `/evidence check`, `/todo` and
  `/review-file` expose the owning evidence. Plain completed answers remain quiet. These are
  observations from `turn_summary`, not permission to retry or replay completed work. Turns that
  failed on a provider or transport error suggest `/retry`.
  Recorded verification remains visible even when no files changed. `Next:` prioritizes retained
  failure/effects, outstanding obligations and verification gaps before optional review/undo.
  Continue only unfinished work after inspection; the footer never authorizes replay of the turn.
  Notes are labelled `Blocked` only for explicitly blocked tasks. Attention opens the exact blocked
  step; plan evidence uses neutral observation markers, not passing-check symbols.
- Settled test and build commands (`cargo test`, `cargo clippy`, `make test-*`, `pytest`, `tsc`, …)
  show a **Test** or **Build** card instead of their raw log: counts at the right of the header, then
  only failing test names or the first errors with their `file:line` locations. The expand key
  (`Ctrl+O`) shows the ordinary card with the complete output. Output the card cannot attribute (a
  crash with no test or error lines, a job moved to the background) keeps the ordinary card.
- Resuming displays durable todos and recorded check statuses; these are history, not fresh test
  results. Messages Dial injected into the conversation are not shown as your text. Completed tools are not replayed automatically. `/review` reviews a frozen local or GitHub
  PR scope with source-backed findings (the captured diff is split on `diff --git` file boundaries into
  chunks of at most 20,000 bytes, byte-splitting only a single larger file; chunks run concurrently as
  read-only `reviewer` tasks up to `task.max_concurrency`, and an interrupt cancels all of them. Findings are
  merged, exact duplicates (path, range, title) dropped, and sorted by priority then confidence. The report
  shows target identity, an unchanged/stale line, a verdict (`do not ship` for any P0/P1; else `inconclusive`
  if any chunk failed or was inconclusive; else `ship with fixes` for P2/P3 only; else `ship`), counts per
  priority, and findings grouped P0..P3, followed by the same data as machine-readable JSON);
  `/review-file` opens the latest changed turn's file browser directly. Every row shows its writers
  (`main` or a worker label; older checkpoints show `unattributed`), overlap signals and whether that
  exact version was kept as reviewed. Selection, writer/overlap filters and review marks survive
  returning to the browser and reopening it in the session; later edits invalidate review marks.
  **Choose turn** explicitly opens older turns. File previews offer **Next file / Previous file**
  within the filtered list without returning through menus. Reviewing is not testing or acceptance.
  Filter to a writer to offer **Revert writer's unshared files**. Unshared files were changed only
  by that writer and in no later checkpointed turn; overlap files stay excluded regardless of filters.
  Group revert first shows the combined diff
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
  previews one file with **Keep reviewed version / Revert file / Next file / Previous file / Back to files**. The review also says how many shell
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
  Owned-work waits instead name awaited jobs/workers/questions and show a separate elapsed wait
  clock, with no invented job percentage or ETA. Results or messages resume the turn without model
  polling; new input changes direction. Interrupting ends the agent turn, not the background job.
  Use Activity (`/activity`) to inspect and explicitly cancel owned work. Provider retry status shows
  its observed attempt and the delay until the next request, reading **retrying…** once it is due;
  `/retry` skips that delay,
  while the interrupt key stops the agent request. Neither operation replays completed tools.
  `/model restart` explicitly confirms unloading only Dial's managed local main model, which reloads
  with the same selection next request. Busy turns, running children and external endpoints are
  refused; the command never switches models or kills external servers. Failed/cancelled inference
  on a Dial-managed server retires its process before retry so abandoned work cannot block the next request.
- `/plan [on|off]` enforces read-only planning. The model's `exit_plan` request asks for approval
  before enabling edits. `/permissions` lists scoped rules; for example,
  `/permissions allow --persist bash cargo test` persists a project rule outside the workspace.
  Deny rules take precedence, including in automatic approval mode.
  Approval prompts lead with the action and show the exact policy-derived reusable matcher and
  lifetime before consent. Answer keys are ignored for about 350 ms after a prompt appears. `Esc`
  or `n` denies; `Ctrl+C` denies and stops the turn. Choosing the project scope asks for
  confirmation because it persists. A batch of changes lists one line per change with
  **Approve all**, **Choose one by one** and **Deny all**. Plan approval reads **Review plan** with
  **Approve plan** and **Keep planning (give feedback)**.
  Shell rules match the displayed normalized argv prefix; later arguments may vary, while
  destructive forms remain excluded. Session grants are not saved; project grants persist into
  future sessions until revoked. Unsupported shell shapes, destructive commands, directory-access
  expansion and application-watch starts offer one-time approval rather than a fictitious reusable
  grant. Capability expansion still requires its existing explicit consent.

## Workspace rail and live worker cards

The optional **workspace rail** above the composer shows per-file additions/deletions, including
pre-existing changes; new files are labelled `new` and binary diffs `binary`. It starts off.
`/changes` lists the cached workspace snapshot.
**Pinned workers** sit above the composer and show running and recently finished workers: a header with the active and done counts, up to three rows in collapsed mode, a live line with the latest output (when `ui.worker_live_preview` is on) and an elapsed clock after five seconds. Workers that finished needing input or blocked are marked as needing attention, stay pinned until you open them and can be resumed from their transcript. `ui.pinned_workers` selects `collapsed` (default), `full` (more rows) or `off`. The block is hidden on terminals shorter than 16 rows. The `workers` status segment shows `running/total workers`, adding `· $cost` once a worker reports cost; the block, the segment and the hub read the same snapshot, and the hub shows each worker's result. Unread worker messages show as `✉ N msgs`. When a worker finishes, needs input or fails, one short notice names the worker, quotes the first line of its question or blocker (needs input, blocked or failed), and points to `Alt+A`; the worker view header then says what continues it (reply below, or see its last message when it cannot be continued); notices raised while the hub or a worker view is open are kept rather than lost. `Alt+A` opens the worker hub. `Alt+Shift+A` opens the worker named by the newest notice; otherwise it opens the transcript of the most recently active running worker, and pressing it again cycles through the running workers. With none running it cycles through all workers, and with none it opens the hub. Double-Left on an empty composer also opens the hub. In worker views, `Ctrl+C` stops the selected worker after a confirming second press. Pinned rows are not clickable: mouse capture is enabled only while a full-screen panel is open. Rebind `app.subagents.recent` to change the transcript key.
A task card settles when the task call returns; workers still running then read *continuing in background*.
Toggle the block in `/settings` → Appearance → Display. The command palette (`Alt+K` or `F1`) and the empty slash menu start with everyday
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
