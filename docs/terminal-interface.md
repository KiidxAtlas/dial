# Terminal interface

Everyday use of the interactive terminal UI: composer, slash commands, shortcuts, settings panels, model settings, themes and previews. Rendering internals are described in [tui-rendering.md](tui-rendering.md).

For independent Dial sessions in the same checkout, `/peers` lists live peers and your own ID;
`/peers send <id> <text>`, `/peers claim <paths...>` and `/peers clear` exchange messages and
manage advisory path claims through ordinary transcript notices. Commands wait until an active
turn finishes. See [Session coordination](session-coordination.md) for delivery, safety and opt-out details.

## Interface overview and shortcuts

- Inline, scrollback-native transcript with a live composer; finished turns remain in normal terminal
  scrollback. Model, provider and settings panels use a full-screen overlay and restore the terminal
  when closed. Approval prompts and settings adapt to narrow terminal widths.
- User and assistant messages have muted UTC timestamp headers. Live timestamps mark submission
  or the start of an answer; resumed messages use their recorded journal time (assistant journal
  times mark persistence). Older imported messages without a recorded time remain unstamped.
- Finished-turn footers show measured output speed as `tok/s` when available, alongside elapsed
  time and token counts. Unknown or invalid rates are omitted; `ui.show_turn_stats = false` hides
  the footer. Historical rates are not reconstructed from message timestamps.
- New conversations open on a centered launch screen: a braille knob and wordmark (the needle turns
  into place at start-up when `ui.motion` allows) above a compact input card near the window
  midpoint. One line beneath shows the model, current directory and Git branch, then a quiet
  shortcut hint; setup and unsafe-mode warnings appear only when relevant. `Down` reveals suggested
  requests or recent sessions; `Enter` opens a request for editing or resumes a session. Small
  windows shed artwork before warnings and selected actions. Sending a message restores the
  normal conversation.
- A configurable status line shows model, role, path, Git state, context, token usage/rate, approval,
  sandbox and session state. Settings let you choose presets or define left/right segments. Learning diagnostics are available in the explicit improvement views.
- Local inference shows **loading model**, **processing prompt**, then a live **generating tokens**
  counter and tokens/second in the composer. Dial's MLX runners report engine counts, including
  reasoning and tool tokens; the text runner also reports prompt processing progress. Other local
  servers use estimates marked `~` until reported usage arrives. Counters reset for each request.
- Active work has a dedicated row above the composer: the current operation, elapsed **turn** time,
  and a compact activity track. Reported prompt counts fill the track; unknown progress uses a
  travelling marker, not a guessed percentage. The metadata border stays stable and the row
  disappears when idle. Custom status layouts opt in with the `spinner` segment.
  Loading, prompt processing and generation all report **no progress for Ns** after 15 seconds
  without a progress event. This is a telemetry-silence warning, not proof that inference has hung;
  local first-event waits remain unlimited by default. Stopping takes precedence over old telemetry.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.
- **Action palette:** `Ctrl-P`, `Alt-K` or `/palette` starts with a short list of goals: start or
  continue work, review changes, manage ongoing work, configure Dial, or browse all actions.
  Choose a goal to drill into relevant commands; return to the goal list before closing.
  Type at any point to search every registered action, including advanced commands and enabled
  skills, without replacing your chat draft. Command names and task-oriented labels both match.
  The selected action shows its exact slash command, description and, while busy, whether it
  runs now, waits or stops the turn. Required arguments are collected before execution.
  Pasting never runs an action. Provider discovery waits until Models or Providers opens.
- **Pickers and `/help`:** run bare, `/approval`, `/effort` (`/reasoning` and `/thinking` too), `/profile`,
  `/theme`, `/queue` and `/permissions remove` open a searchable list in the TUI with the current value
  marked; `Enter` applies it and `Esc` cancels. `/queue` rows edit on `Enter` and remove on `Delete`,
  `Backspace` or `x` (on an empty search). The argument forms still work in scripts and text output.
  `/help` opens the goal-first palette; `/help text` prints the command list and `/help all` includes
  the advanced sections. The footer lists the keys the current mode uses; Esc clears before going back.
- **Work plan:** the live summary shows the current step, blockers and the next step rather than
  repeating the whole checklist. `/todo` opens a searchable inspector, ordered by current work,
  blockers, upcoming steps and closed history. Select a step for its full recorded reason,
  finding and evidence; Back returns to the plan. Browsing never changes task state.
  `/todo list` retains plain-text output; `/todo show <number>` opens a recorded step directly.
- A frameless default look (`dial-dark`/`dial`): shaded bands for your messages and the composer,
  a shared icon gutter for answers and tool calls, and inset panels and menus with a filled
  selection. Dark, light, monochrome, Gruvbox, Nord and Paper themes; custom JSON themes, symbol
  presets, band, box or line composer, styled headings/code/diffs, expandable tool results, and
  `NO_COLOR`.
- Activity reads apart at a glance. Commands, code cells, edits and web calls are framed cards
  with the exit code and time on the bottom edge. Runs of reads and searches fold into `Read (N)` /
  `Search (N)` trees. Notes sent to other agents show as quoted `✉ IRC` lines with delivery
  receipts. Skill and tool loading stays in the quiet `◆` context lane.
- Unicode-aware multiline editing, bracketed paste, persistent per-project prompt history and history
  search. Secret provider inputs are masked and never enter prompt history.
- Pasted text stays in the draft. Buffered unbracketed paste preserves line breaks, and an immediate
  trailing Enter after bracketed paste is ignored. Press Enter after the paste settles to send it.
- **Clipboard images:** the paste-image key (`app.clipboard.pasteImage`) attaches the clipboard image
  as a quoted PNG path in the draft. Sources by platform: macOS reads the pasteboard (PNG, TIFF,
  JPEG, including screenshots) and then an image file copied in Finder; Windows and WSL read the
  Windows clipboard through `powershell.exe`; Wayland uses `wl-paste` and X11 uses `xclip`. Termux
  has no clipboard image API (`termux-clipboard-get` is text only) and reports that explicitly.
  Every image is normalized to PNG and downscaled to fit the 5 MiB attachment limit. If nothing
  usable is found, the error lists each reader that was tried and why it produced no image.
- Questions and approvals quarantine input queued before the dialog opens, so a buffered Enter
  cannot approve an unseen action. A question opened over a nonempty draft starts with draft
  focus: Enter edits that draft, Tab/Escape moves to the choices, and Ctrl+C cancels the question.
- **Inline suggestions:** command arguments offer valid choices as dim ghost text even before you have
  used them: `/understanding ` suggests `architecture`, `/skills s` suggests `sources`, and
  `/skills enable ` suggests an installed skill name (including disabled skills). Built-in choices
  take priority over matching history; ordinary prompts still suggest the newest matching history entry.
  `Tab` or `Right` accepts the ghost; `Esc` hides it; `Enter` sends only what you typed. Suggestions
  appear only at the end of an idle draft, never in secret prompts or mid-line.
- **Slash commands:** typing `/` opens a list of every command (built-in and your `.dial/commands/*.md`)
  that filters as you type. `Up`/`Down` move (wrapping, with a position counter for long lists), `Tab`/`Right`
  complete, `Esc` dismisses without losing the draft. `Enter` sends exactly what you typed, so a full command
  runs at once; accept a highlighted suggestion with `Tab`/`Right` first. File mentions (`@path`) are the
  exception: `Enter` completes the highlighted file. Built-ins win over a custom command with the same name.
- While a turn runs, submitting a non-command draft opens **a message-intent chooser**. Nothing is
  sent just by opening it; Escape returns to the unchanged draft. The options are:
  **Send now** (by default steers this task with no model call; `[message_routing] route = "classify"`
  lets a routing judge decide between steering and an independent worker, and a failed decision steers),
  **Send after this turn** (queued, unchanged), **Steer now** and **Independent worker**.
  **Steer now** is an explicit redirect: it cancels the pending model request or discards its stale
  response before any of its tool calls run. A tool already running may finish (only waits that
  are safe to interrupt are interrupted); unstarted actions are skipped and completed results are
  retained. The latest instruction takes precedence over conflicting older plans.
  **Independent worker** starts a separate worker for exactly that message while the current task
  continues; it never steers the running task and no model classifies it. If a worker cannot
  start (spawning disabled, capacity, interruption) the message is queued as a follow-up instead.
  Explicit messages are shown above the composer, one row each (`○ Steer  pending · acts at the
  next safe point  “message”`, with ● received and ✓ applied), with the stage Dial actually reported:
  *pending* (typed, not yet read by the agent; it acts at the next safe point), *received*
  (the agent took it and is cancelling the stale reply) and *applied* (recorded as the current
  instruction, or the worker started). If the turn ends before a message was applied it is queued
  to send next rather than lost. They are not part of `/queue`.
  `Option+Enter` on macOS (`Alt+Enter` elsewhere) still queues a follow-up directly.
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
  reasoning visibility, tool expansion, screen reset and dictation remain available. Escape first
  dismisses a local completion/search/send chooser; a subsequent Escape cancels the wait. Screen
  reset works immediately during inference instead of waiting for the turn to finish.
- In `/models`, highlight a model and press `Ctrl+F` to favorite or unfavorite it.
  The **Favorites** view lists saved favorites; `*` marks favorites that are not the current main model.
  Favorites use the picker's global/project scope (`Ctrl-S`) and persist across restarts.
  In All models, favorites are pinned below the current main model and above other models.
  Press `Option+]` on macOS or `Alt+]` elsewhere (remappable as `app.model.favoriteNext`), or use `/model next` to cycle through
  available chat favorites without opening the picker. Unavailable/embedding models are skipped;
  a switch requested during a turn saves immediately and is adopted when the turn finishes rather than interrupting it.
  On the first press, if discovery is needed, the switch is retained and applies when ready.
  Escape or `/model cancel` cancels that pending switch. Changes to the selected model, saved
  favorites, scope, or discovery request also cancel it rather than applying a stale choice.
- `F8` edits a model's HTTP headers for the role being picked (`Roles`, then `Enter`, then `F8`). The editor title names the model and role, and saving assigns only that role. Outside a role pick, `F8` asks you to choose a role first, so browsing never reassigns main. Inside the editor, `F9` inherits the provider's headers (`Tab` only moves between fields, so it works on legacy terminals). Below 56 columns the groups column is hidden and cannot take focus.
- Model lists in `/models` filter and sort without leaving the list. `F2` cycles the **show** filter (any, available, loaded, local, cloud, tools, vision, reasoning, priced, used) on top of the kind tabs and search; `F3` cycles the **sort** (default ranking, name, cheapest, fastest, largest context, most used; unknown values sort last). A non-default filter or sort is shown next to the kind tabs, and `Esc` clears the search first, then resets filter and sort, then closes.
- Each model row shows its context window, then on wide panes the median recorded speed (`t/s`) and the provider price as `$input/$output` per 1M tokens (`free` for zero-priced models). The detail lines add max output tokens, local or cloud, the roles using the model, and a numbers line: price per 1M tokens (input, output, cache read and write when the provider reports them), the speed median with its sample count and trend, average cost per priced request, request count, average latency, error rate and total recorded cost. Usage comes from the stats database (`dial stats`), read when `/models` opens and again on `F5`. Unknown values show as `-`; nothing is estimated. Requests are recorded under the backend's model id, so local checkpoints served by Dial's own engine share one label and show `-` per model.
- **Memory** is the group after Favorites in `/models` (`/model memory` opens it directly). It lists the chat models that can serve the `memory` role, leads with the one assigned, and its header shows the active memory backend and what the `memory` role is set to. Enter assigns the highlighted model to `memory`; `Ctrl+G` opens Get models on its Memory shelf. Picking the `memory` role from Roles also lands here.
- **Images** lists discovered image-generation models, including compatible checkpoints already downloaded for MLX serve. Enter assigns an image model to the `image` role rather than replacing main. Choose the role from **Roles**, or use `Ctrl+A` on a compatible model. When unassigned the image role displays **follows main**; a vision-input model is not automatically an image generator. **Get models → Images** browses Hugging Face MLX text-to-image repositories; searching reads metadata only, and Enter explicitly downloads a chosen result with the normal cancellation controls. Downloaded image models are kept out of chat and embedding role choices.
- **Get models** follows Images in `/models` (`/model local` and `/models local` open it directly). Its **MLX**, **GGUF**, **Memory**, **Images**, **Embeddings** and **Curated** shelves switch with `Tab` or `Right`; `Left` and `Esc` return to groups. Suggestions precede installed models. Type a query and press Enter to search Hugging Face; Enter on a result lists its files and explicitly downloads it in the background (GGUF asks which quantization first). Installed chat models assign main, or `memory` on the Memory shelf. Images downloads go to the Hugging Face cache and require a compatible image server/pack; they do not run through Dial's chat runner. Serve/import them, then F5 rescan and choose their endpoint in Images. The Embeddings shelf browses Hugging Face embedders; Curated lists discovered embedding endpoints and curated Ollama setup choices. Esc cancels a running download first. Downloads keep running while browsing other groups; closing `/models` cancels them. See [model-downloads.md](model-downloads.md) and [model-embeddings.md](model-embeddings.md).
  Opening `/models` loads no model, starts no Python or MLX process and makes no network call; the installed list is read when it opens. Search, listing and downloads run only after an explicit key press, in the background. Switching to the Embedding shelf reads Ollama's model list from localhost. The same installed data is printed by `dial models status` (`--json` for the machine-readable form, with identical field names). On short panels the details give way to rows, and the list always keeps at least one row.
  - **Speed** is output tokens per second of request wall time from the recorded stats, the same measure as
    `dial stats`. It uses the newest 40 successful requests with output and timing: a median, the median of the
    newest 20, and a trend (`up` or `down` when that median moves more than 10% from the 20 before them, else
    `flat`). It includes prompt processing time, so it is not a pure decode rate.
  - **Fit** reuses the `dial models recommend` estimate (configured or default 32768-token context, detected
    memory), with the layout from `config.json` for MLX and from the file header for GGUF.
  - **Version, adapter, gate** come from the learning store for that model under the current project's state
    directory, read with SELECT statements only. When several use cases share a model, the store changed most
    recently is shown.
  - **Waiting** lists the recorded blockers from the same rules as `/improve status`: verified turns since the
    last cycle against `[improve] lora_min_examples`, new turns against `min_new_turns`, and the remaining cooldown.
    Background-only gates (idle, AC power, `/compute` pause) are not read here, so an empty list means no recorded
    blocker, not that a cycle will start.
  - Anything unavailable is shown as `unknown` (JSON `null`): no stats database, no matching learning store, an
    unreadable store, or no memory reading. It is never estimated from a neighbouring value.
- `/sessions` and `/resume` open the current project's conversation picker. Pinned sessions lead,
  then date groups (Pinned, Today, Yesterday, This week, Older; headers are not selectable), each
  ordered by journal activity rather than creation time. Empty sessions are hidden unless pinned;
  **Tab** includes them, and the hidden count appears only when some are hidden. Titles take priority
  over IDs and model details; the selected conversation shows its latest prompt and answer, and in
  wide terminals (100+ columns, 18+ rows) a preview pane shows its first and last turns. Search
  matches words in any order across titles, prompt/answer previews, model and ID, with exact title
  matches first. The header reads `showing N of M` when the first page (200 per kind) does not hold
  every journal; **Ctrl-A** loads all of them, and a search that matches nothing in the loaded set
  says so.
  Keys: **Ctrl-U** clears search; **Esc** clears a non-empty search first and closes on the next
  press; **Ctrl-P** pins/unpins; **Ctrl-R** renames the selected session (edit the title in the
  search line, **Enter** saves); **Ctrl-F** forks it; **Ctrl-D** deletes it after confirmation (the
  current session is deleted with `/delete`); **Ctrl-G** toggles this project and all projects (all
  projects shows each session's project directory; resuming a session from another project is
  refused with a pointer to `/move`); **Home/End** and **PageUp/PageDown** navigate; **Enter**
  resumes. A session another Dial process has open is marked `[open elsewhere]`; choosing it creates
  an independent sibling fork and identifies the source, without taking over its writer or changing its bytes.
  `/resume <id-prefix>` opens a unique ID directly, including empty sessions; an ambiguous prefix
  opens the picker with that prefix as its search. Choosing the current session notes
  `Already in session`. After a resume, the note shows the title, model, last activity and the
  recorded state. `/tree forks` shows fork depth across sessions: each fork is indented under its
  parent, and siblings are ordered most recent first; it includes empty sessions initially. The picker
  keeps up to 200 conversations and 200 empty sessions; listing occurs once per open, and each action
  reloads the list rather than running per keystroke.
  `dial --restore` opens the parent conversation first, then explicitly rebinds eligible interrupted
  workers through the normal safe resume path. Use `-r <id> --restore` to name the parent; without
  an ID it continues the latest conversation. It does not resubmit the main prompt, replay completed
  calls or restart completed/cancelled workers. Refused workers remain interrupted with a reason.
- `/tree` opens the session's own entry tree: every user message and labeled entry, including
  branches abandoned by a rewind, indented where the conversation forked (filled markers are on the
  branch the model sees, hollow ones are abandoned; `<- here` marks where you are). Type to filter
  by text or label, **Enter** moves there. The preview uses the conversation's native transcript
  renderer, including tool evidence. **Ctrl-Up/Down** jump between user turns, **Ctrl-Left** goes
  to the visible parent, and **Left/Right** select siblings. Choosing a user message rewinds to just
  before it and loads its text only when the composer is empty; an existing draft is preserved.
  Choosing a labeled answer continues from it. The move appends a branch marker, so nothing is
  deleted and the previous path stays in the tree. This is conversation navigation, not file rollback;
  use the checkpoint/review controls to change workspace files.
  `/label <text>` bookmarks the latest message (up to 60 characters; `/label` alone clears it;
  the newest label on an entry wins). Labels are journal records the model never sees. Session
  journals carry an integer `schema_version` in their header; older journals load unchanged and a
  journal from a newer Dial is refused with a message to update.
- `/btw <question>` starts a streamed, tools-free side conversation without stopping the main turn.
  Busy questions use the frozen turn-start context, including its active user prompt; they do not
  read the running agent's mutable state. Follow-up questions continue the open thread, and
  `/btw new <question>` starts another. `/btw threads` lists them, `/btw open <id>` displays one,
  `/btw copy` copies its last completed answer, and `/btw cancel` stops only the side request.
  Stopping the main turn does not cancel a side answer. Saved side history is private and bound to
  its parent session; it is not injected into the main model conversation.
  Admitted questions and streamed prefixes are checkpointed before display. Reopening after a
  crash retains the observed prefix as an interrupted exchange and does not repeat the request.
  The private sidecar follows session move/archive/delete operations; independent forks do not
  take over a live side request.
  `/btw promote <id>` creates a full sibling conversation from the saved ancestry and completed
  exchanges. It leaves the main branch, draft and jobs unchanged and returns a `/resume` target.
  Promotion waits for an idle main owner; ordinary side commands run while it is busy.
- `/branch` with no argument opens a list of the nine most recent user messages (keys 1-9);
  `/branch <n>` names one directly. Either way the rewind asks first and says how many messages
  leave the view. `/delete` and `/session delete` ask before removing the current session file;
  cancelling keeps it.
- `/rename <title>` names a session; `/export [path] [--html]` exports Markdown or self-contained
  HTML without overwriting existing files. `/copy` copies the last answer; `Ctrl-G` opens the
  draft in an external editor.
- `/pack [path]` writes the same archive as `dial session pack`: one zip with the journal, side
  history, stream journal, subagent transcripts and referenced artifacts, each recorded in a manifest with
  a SHA-256 and byte count. `dial session unpack <file>` (or `dial sessions unpack`) installs it into the
  current project. Unpacking verifies every member and artifact before anything is written, refuses path
  traversal, absolute or backslash names, symlinks, oversized members, corrupt bytes, and archives whose
  format or journal schema is newer than this Dial, and never overwrites an existing file. The transfer is
  recorded, so reopening the original path after the session moved away forks a sibling instead of
  reattaching to it.
- `/export` and `dial sessions export` (Markdown or HTML) and `/dump all` include the subagent transcripts
  that belong to the session, after the parent conversation and with the same redaction. `/dump anon`
  anonymizes them the same way it anonymizes the session.
- `/share` and `dial sessions share` seal the transcript under an 8 MiB cap. When it does not fit, they
  trim in order: inline images are replaced with an `unavailable` marker, long strings are truncated at
  32768, 8192, 2048, then 512 characters, and only then the oldest records are dropped (the session header
  is always kept). Each trim applied is reported. A configured `[share].server_url` (or
  `DIAL_SHARE_SERVER`) receives the share; without one, or with `--gist`, it becomes a secret GitHub gist
  created through `gh`. If `gh` is missing or unauthenticated the command fails with an actionable error
  and does not fall back to another host.
- A turn that edited files, was interrupted or failed ends with a **Turn report** note: outcome, changed
  files, recorded checks (and whether they are current with the final edits), open tasks, what `/undo`
  can revert, and which shell or extension calls ran outside checkpoints and cannot be undone. Plain
  answers with no edits show no report. The summary is stored in session control state as `turn_summary`.
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
  checkpoint kept for `/undo` (which would overwrite those files, so review them first). One
  `/undo` checkpoint covers the revert. A file written by several writers, or changed again in a
  later turn, is marked *overlap*: that is a risk signal, not proof either way, so it is never part
  of a group revert and a single-file revert asks for separate confirmation. No recorded overlap
  does not prove a file is
  independent of other code (a renamed function and its callers). `/review-file <turn> [path]`
  previews one file with **Keep / Revert file / Cancel**. The review also says how many shell
  commands and extension calls in the latest turn were not checkpointed: their effects cannot be
  shown or reverted here. Files too large to checkpoint are listed as not reviewable.
  A revert includes all changes shown for that file, including your own edits: review carefully.
  Changes after preview are refused, other files are untouched, and the revert has its own `/undo`
  checkpoint. Private, binary or oversized interactive diffs require manual review.
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
- Idle terminal frames are not rewritten. Fenced code gets theme-aware lexical highlighting,
  and a `diagram` fence (```` ```diagram Optional title ````) is drawn as a filled, framed panel: an
  accent border with the title in its top edge, connector glyphs tinted apart from the labels, and the
  theme's code background. Plain ASCII line art is redrawn cell for cell: `-`/`=` runs become `─`/`═`,
  `|` becomes `│`, `->` and `<-` get `▶`/`◀` heads, a lone `v`/`^` under or over a line becomes
  `▼`/`▲`, and each `+` becomes the matching rounded corner or junction. Hyphens and letters inside
  words stay labels, and the ASCII glyph set shows the source unchanged. Diagram rows are clipped
  with an ellipsis rather than wrapped, so their columns stay aligned; the source text is unchanged
  in the transcript. The agent is advised, not required, to use a diagram only when the structure
  branches, loops, layers or links several parts, and to write it as unboxed ASCII arrows: boxes and
  long arrows cost tokens without adding meaning, box-drawing characters cost more than ASCII, and a
  linear flow is shorter as one sentence or one arrow line.
  Completed Markdown prefixes are cached while streaming. Edit and write cards show the change as a
  numbered, coloured diff (separate hunks with two lines of context, per file); the anchored preview rows
  and removed-lines report the model gets are left out. Each todo call renders the list as it now stands,
  with the steps that call closed struck through and emphasized. Passing automatic checks are one line.

| Action | Shortcut / command |
|---|---|
| Search slash commands | `Ctrl-P` / `Alt-K`, `/palette` |
| Model selector / roles | `Ctrl-K` on macOS (`Alt-M` elsewhere) / `Alt-R`, `/model` |
| Provider setup | `Alt-P`, `/providers` |
| Settings | `Ctrl-,` (`Ctrl-4` fallback), `/settings` |
| Edit keyboard shortcuts | `/hotkeys`, or `/settings` → Setup → Keyboard shortcuts |
| Previous/next prompt | `Up` / `Down`; preserves the unsent draft |
| Search prompt history | `Ctrl-R` |
| Command list / completion | `/`, then `Up` / `Down`, `Tab` or `Right` to complete, `Esc` to dismiss |
| Accept inline suggestion | `Tab` or `Right` at the end of the draft |
| Multiline input | `Option+Enter` on macOS / `Alt+Enter` elsewhere; `Shift-Enter` when supported by the terminal |
| Expand/collapse tool results | `Ctrl-O` |
| Scroll transcript | `PageUp` / `PageDown` |
| Cancel a panel / stop at next callback | `Esc` / `Ctrl-C` |
| Quit from an empty composer | `Ctrl-D`, `/exit` |

Dial detects the OS for the model shortcut: on macOS, **Control+K** works without terminal
configuration (not Command+K). Option+M remains available as a fallback. Other Option shortcuts on
macOS require Option to send Meta/Esc; in Terminal.app enable **Use Option as Meta key**.
Without that setting, Option+M may type `µ` instead. `/model` works on every platform, and
the footer and `/hotkeys` show your active bindings, including custom overrides.
Shortcut labels use **Option** on macOS and **Alt** elsewhere. **Ctrl** always means Control,
not Command. Both `Alt` and `Option` are accepted in keybinding configuration on every platform;
saved bindings retain the portable `Alt` spelling. `/hotkeys` shows the effective native labels.

`/hotkeys` opens the same searchable shortcut editor while idle, during a turn or while waiting for compute.
Type to filter by action, group or key; `Enter` edits, `Delete` restores defaults, and `Esc` goes back.
Enter chord names separated by spaces, such as `Alt+M Ctrl+K`; an empty field disables the action.
Changes save globally and apply immediately, only after validation and a successful write. Conflicting
bindings are rejected, and the editor keeps a way to confirm and close panels. You can also edit
`~/.dial/keybindings.toml` directly, for example:

```toml
"app.model.select" = "Ctrl+K"
"app.history.search" = []
```

### Settings

`/settings` is the configuration hub. Type to search labels, descriptions and configuration paths
across every category; search words can appear in any order. Exact configuration paths rank before
fuzzy matches. `Left`/`Right` change categories and `Tab`/`Shift-Tab` jump sections.
`Enter` toggles a two-state switch or opens an editor/choice list.
Settings with an automatic mode open an explicit **Auto / On / Off** picker at the current value
instead of cycling and saving immediately. Choices save only when confirmed; `Esc` cancels without
saving. `Ctrl-S` switches **All projects** / **This
project** storage; the title shows the destination and successful saves are acknowledged. `Delete`
removes the selected override so the inherited value applies. The detail area identifies the effective
source and restart requirements. The **Setup** category opens models, providers and keyboard controls.

While you search, the category tabs are not offered and `Tab` does nothing; `Esc` clears the search first.

Choice lists also accept typing to filter their labels and stored values. `Esc` clears that filter
before returning to settings; `Enter` confirms only a visible match. Clearing an override restores
its inherited value, rather than resetting unrelated settings.

Settings that name something Dial can list offer it instead of a typing field; `Ctrl-E` in any of
these choosers switches to typing the value, and every chooser still accepts values it does not list:

- **Model rows** (draft model, architecture model, main model, decision-memory override, image
  model) list discovered and downloaded models. A suggested local model that is not downloaded is
  marked `· download`: the first `Enter` asks, the second downloads it in the background and saves
  the setting only when the download succeeds. A failed or cancelled download (`Esc` closes the
  panel and cancels it) keeps the previous value; opening a picker never starts a download. The
  main model row assigns the model through the same path as `/models`.
- **Name rows** (provider, model profile) list the configured names.
- **Path rows** (Python, Node, worker, IDA, LSP programs) open a file browser starting in the
  project. `Enter` opens a folder or chooses the entry; typing filters the listing, or type a path
  (`~/`, absolute or relative) and press `Enter`. Hidden entries appear when the filter starts with `.`.
- **Known-value lists** (status line segments, compaction order, integrations, foreign rule
  sources, recall types, learn-from models) open a checklist: `Space` checks or unchecks,
  `Shift-Up`/`Shift-Down` reorders preference lists, and the last row adds another value. Extra
  writable folders add entries through the folder browser.
- **Tables and arrays** (verification, model profiles and similar collections) open one
  row per entry: `Enter` edits that entry as `name = value` (or one value for arrays), `Delete`
  removes it, and **Save** writes the collection. Project scope edits only the project layer, as the
  text editor does. Collections that can hold credentials (providers, MCP, hooks, tools, LSP and
  similar) keep the masked text editor.

The hub includes controls for all persisted configuration fields. `/settings` manages interactive/coding
settings; `/improve settings` holds the learning and evaluation categories. Less common scalar fields
and complex collections have explicit configuration rows: provider definitions, model profiles and
presets, agents/custom tools, MCP/LSP/debug adapters, hooks, verification, budgets, training and voice.
Search by a descendant path such as `providers.custom.headers` or `browser.allowed_hosts` to find
the complete structured editor. These advanced rows accept TOML inline tables or arrays (including
arrays of tables), not JSON. For example, browser host restrictions use `{ allowed_hosts = ["example.com"] }`.
Structured editors start with only the selected storage layer, not inherited global values. Saving
replaces that layer's value; untouched inherited fields continue to merge normally. Hooks retain their
append-across-layers semantics. `Delete` removes the selected layer's override. The controller validates
the full configuration before writing; failures keep the editor and its draft open.

Credential-bearing collections and literal secret lists are masked while browsing and editing.
`Ctrl-R` deliberately reveals/hides a sensitive editor; opening another editor always restores masking.
Prefer environment-variable references to literal tokens. Runtime-only paths, derived context and
private account policy are not editable settings. The retired `ui.hyperlinks` compatibility key is not
offered, and reserved MCP switches are explicitly labeled as having no current runtime effect.

### Local and hosted setup presets

`/setup presets` offers **Local only**, **Local with approved hosted assistance**, and **Hosted**,
then **This project** or **All projects** scope. You can also open a specific review with
`/setup presets local-only project`, `local-assisted`, or `hosted`. Nothing is saved until the
final confirmation. Choose the main model in `/models` first; assisted setup also requires an
explicitly assigned hosted coach in `/roles`. Setup never picks an unconfigured model, probes a
provider, downloads weights, or changes credentials.

The main model stays unchanged. Small/compaction, task, tester, planner, memory and judge roles
use that model; the coach does too except in assisted setup. Local presets require an on-device
runtime or loopback endpoint, not merely a provider named “local” or a LAN address. They enable
the lean tool/context profile. The review shows detected physical RAM, context and role choices;
`dial models recommend` provides weight/KV memory-fit estimates rather than guessing from RAM alone.

All three presets clear automatic fallback chains, select local provenance memory (no embedding
or memory-model requests), and disable Laya, teacher assistance, distillation and automatic
escalation. Assisted setup enables the reviewed hosted coach and sets **Ask** approval, preserving
**Readonly** if selected. Approved coach questions disclose their supplied evidence to that provider.
The review lists these changes before saving. Sandbox and read-before-edit protections are unchanged.
Conflicting project/CLI overrides fail before saving; global defaults are also validated without
the current project's overrides. Presets are durable configuration bundles, **not a network firewall**:
explicit later model/profile choices, custom agents and network tools retain their own policies,
and other projects can override global defaults.

`/model settings` (alias `/model-settings`; also `F7` in `/model`) tunes the **current main model**.
Settings are grouped into sections, with a description and the accepted range for the selected row.
`↑/↓` move, `←/→` cycle a choice or toggle and step a number (the first press starts from a sensible
value), `Enter` types an exact value, `r` resets a row to its default, `s` switches between project and
global storage, `d` opens shared context defaults, and `g` opens all shared settings. Changes validate against the model's declared
capabilities and save immediately, applying on the next request; a dot marks values you changed.
Profiles match an exact provider/model pair, so switching models never reuses another model's overrides.
The **Context size** row sets a per-profile token cap (2,048–2,097,152), taking precedence over shared
context settings while still respecting model/server capacity. Reset it with `r` to follow shared
settings again. This controls history and compaction, not an external server's loaded context size.
Use `/model defaults` to open **Local context default** and **Cloud context default** directly.
This shortcut starts in global scope so the defaults persist across projects and model changes;
switch scope to set a project-only default instead. Enter a token count (for example `131072`)
or `auto` to follow model capacity. Per-model overrides and smaller loaded-server limits still apply.
Local MLX models (and LM Studio, with a subset of these controls) additionally expose top-k, min-p, XTC, seed, repetition/presence/frequency penalties with
their windows, and chat-template arguments; LM Studio rejects XTC and chat-template arguments. For an MLX model, every option its
chat template reads (for example `enable_thinking`) gets its own row: flags toggle with ←/→, options the template compares against
cycle through those values, and other options take typed text. **All template arguments** still edits the whole table as TOML.
MLX models also get **MLX server** rows: a draft model for speculative decoding (←/→ cycles installed models that share the
tokenizer; Enter types any other), draft tokens, and KV-cache bits, group size and start offset, picked from their valid values.
Ollama models get a **Keep alive** row. Those request controls are never sent to cloud providers. Startup options of externally
managed servers belong to those servers, not this page.

To create a theme, put a JSON file such as
`~/.dial/themes/custom.json` there; it may extend a built-in (`dark`, `light`, `mono`, `gruvbox`,
`nord`, or `paper`) and overrides reload live. `NO_COLOR` forces monochrome output.

`Ctrl-S` changes global/project storage scope in the model, provider and settings panels.

## Model capability icons and image previews

Model rows include monochrome icons at the right for vision, tool use, reasoning and embeddings, with a legend and capability
labels in the highlighted model's details. ASCII themes use `V`, `T`, `R` and `E`. `?` means metadata is
unknown. Typing `vision`, `tools` or `reasoning` searches supported capabilities as well as names.
Local capability declarations come from downloaded configuration and chat templates; discovery does
not start inference or load weights to produce these badges.

Viewed images and user image attachments also appear as compact inline terminal thumbnails, up to
80 columns by 16 rows, with their filename and original dimensions. Previews preserve proportions,
shrink for narrow terminals and remain in scrollback. iTerm2, Kitty and Ghostty receive a sharper
native image preview; other terminals and multiplexers use portable colored cells. Image decoding
runs on a worker while input and interruption remain responsive. The model receives the original
image bytes independently of preview size; supported cloud image requests use high detail.

## Terminal integration

With `ui.terminal_integration` on (the default; `/settings` → Interaction → Notifications), Dial
tells terminals that support it more than the cells it draws. Each item is sent only to terminals
known to implement it, detected from `TERM_PROGRAM`, `LC_TERMINAL` and similar variables:

| Signal | Terminals | What you see |
| --- | --- | --- |
| Links (OSC 8) | iTerm2, Ghostty, kitty, WezTerm, VS Code, foot, Rio | URLs and file paths that exist, resolved against the project, are clickable in scrollback. In iTerm2, `path:line[:col]` opens at that line through Semantic History (⌘-click, using the editor set in Profiles → Advanced). The live region and text that is not a real URL or path are never linked. |
| Working directory (OSC 7) | iTerm2, Ghostty, kitty, WezTerm, foot, tmux | New tabs and splits start in the project. |
| Tab color (OSC 6) | iTerm2 | Amber while working, red when an approval or question waits or a turn failed, green when a turn finished. A key press clears it. |
| Attention | iTerm2 | The dock icon bounces while an approval or question waits. |
| Progress (OSC 9;4) | iTerm2 3.6+, Ghostty 1.2+ | Prompt processing percentage, an indeterminate bar while generating, paused while waiting for you. |
| User variables | iTerm2, WezTerm | `user.dialState` (`idle`, `working`, `waiting`, `done`, `failed`), `user.dialModel` and `user.dialContext` (context used, such as `42%`) for status bar components, badges (`\(user.dialState)`) and tab titles. |

Under tmux, terminal-only sequences go through passthrough, which needs `set -g allow-passthrough on`.
Links are off there because tmux does not keep them by default. Zellij and GNU screen get none of
these. Dial restores tab color, progress and variables when it exits, suspends or crashes. The older
`ui.hyperlinks` key is still accepted but ignored.

### Live side pane

`/watch` enables a private live feed for the current session and runs immediately, even during a
turn. In a direct macOS iTerm2 session it opens a vertical split targeted to that exact session.
macOS may ask for Automation permission. If unavailable, or under a multiplexer, Dial prints a
shell-quoted command to run in another pane:

```sh
dial -C /path/to/project watch SESSION_ID
```

The pane follows new edit-result previews, changed paths, language-server advisories, turn outcomes
and `/review` results in its own scrollback. Preview diff lines are coloured; this is a display of
tool output, not a new verification result. LSP advisories require a running server under the usual
LSP settings and remain explicitly **not verification**. `/review` keeps its existing execution and
queuing behaviour; opening a watcher does not start a review or model request.

When the pane is attached, screenshots are copied only from verified attachments and shown there,
with a one-line marker in the conversation instead of an inline thumbnail. Native images fit the
viewer's available width and height; unsupported terminals use coloured cells. If publishing fails
or no viewer is attached, Dial keeps the inline preview. Closing the watcher restores inline images
on the next capture. `Ctrl-C` stops the viewer; `dial watch SESSION_ID --once` prints the recorded
feed and exits. Redirected output contains no colour or graphics escape sequences.

Feeds are opt-in per session and stored outside the checkout under the project's private state
directory, `live/SESSION_ID/`. Switching or forking sessions does not redirect an existing viewer;
run `/watch` in the new session. Text previews are bounded and the append-only text feed stops at
16 MiB rather than silently deleting earlier entries; screenshot copies stop at 64 MiB per session.
These files may contain source and screenshots; they are local private state, not published or sent
to an additional service.

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

See [the UX review](ux-review.md) for audit coverage, design decisions and remaining limitations.
