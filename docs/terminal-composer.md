# Composer and transcript

Write, paste and edit drafts without losing your place; understand the launch screen, transcript and status line.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

- Inline, scrollback-native transcript with a live composer; finished turns remain in normal terminal
  scrollback. Model, provider and settings panels use a full-screen overlay and restore the terminal
  when closed. Approval prompts and settings adapt to narrow terminal widths. When the terminal's
  width changes and holds for 150 ms, Dial reprints the blocks still on screen at the new width;
  scrollback above is left as printed and never erased (Ctrl+L reprints the whole conversation).
- User and assistant messages have muted local-time timestamp headers. Live timestamps mark submission
  or the start of an answer; resumed messages use their recorded journal time (assistant journal
  times mark persistence). Older imported messages without a recorded time remain unstamped.
- Finished-turn footers show measured output speed as `tok/s` when available, alongside elapsed
  time and token counts. Unknown or invalid rates are omitted; `ui.show_turn_stats = false` hides
  the timing (the outcome stays). Historical rates are not reconstructed from message timestamps.
- A turn that called tools closes with a **recap card** in the same rounded frame as the tool
  cards. The top edge shows how the turn ended, how many files changed and any open tasks. The body
  holds chips for the verdict of the turn's last test or build (`✓ tests 18 passed`,
  `✗ build 2 errors`) and what the turn did (calls, lookups, edits, commands, web and service
  calls, workers, failures), the files its edits changed with the lines each added and removed (six
  files, then `+N more`), any recorded handoff and, when files changed, `/review-file` and `/undo`.
  Time and tokens sit on the bottom edge. The outline follows the outcome: accent for done or
  answered, amber for blocked or incomplete, red for failed. Plain answers and terminals narrower
  than 40 columns keep the one-line footer.
- Other blocks that need attention are cards in the same frame, each falling back to plain rows on
  narrow terminals:
  - A failing test or build run is a red card (amber when it only warns) with the command on the
    top edge, the failing tests or the first errors with their locations inside, and the counts,
    time and hidden log on the bottom edge. A passing run stays one row.
  - A workers call frames its worker tree. The bottom edge has one chip per state (done, running,
    in background, failed, …). The outline is red when a worker failed and the accent while one runs.
  - A plan update shows a progress bar and `n/m done` on its top edge and a blocked chip on its
    bottom edge. The outline is green once every step closed and amber while one is blocked.
  - Errors, and notices titled as warnings or failures (for example **Outstanding work** or
    **Heads up**), are red or amber cards. Other command results stay labelled rows.
- New conversations open on a centered launch screen: a braille knob and wordmark above a compact
  input card near the window midpoint. When `ui.motion` allows, the mark powers up at start-up:
  the bezel traces itself in behind a bright comet, the needle swings up as the trace passes and
  lights the ticks, and the wordmark tunes in under a travelling glow. At rest the needle breathes
  and a glint passes now and then. The version and tagline type in under the mark when there is
  room. One line beneath the input shows the model, current directory and Git branch. Below it, the
  context stands in rounded cards as wide as the input: setup and unsafe-mode warnings in an amber
  **heads up** card (only when relevant), recent sessions or starter requests in a list card whose
  edges carry its heading and key hint, and the first-launch tour in a **first steps** card.
  `Down` focuses the list and `Enter` resumes a session. Esc or sending a message dismisses the tour
  for good (the marker lives in Dial's private user state). Small windows shed artwork before
  warnings and selected actions, and a card turns back into plain rows when its two edge rows do
  not fit. Sending a message restores the
  normal conversation, and so does a command that prints a result (for example `/add-dir`), so
  its answer is shown rather than left in scrollback behind the launch screen. Commands that
  only open a panel keep it.
- Text that starts with `/` but is not a command is sent as an ordinary message. A first word
  containing another `/` (a pasted path such as `/Users/me/notes.md`) never opens the command
  menu, so `Enter` sends it as typed, and while busy it queues like any other message.
- A configurable status line shows model, thinking level, role, path, Git state, context, token
  usage/rate, approval, sandbox and session state. It keeps the same segments while idle and busy.
  The default layout adapts to width (path drops below 120 columns, branch and cost below 80); every
  width keeps the thinking level (`think high`, `think on`, `think auto`, …), and narrow layouts keep
  the workers segment (with a warning when a worker is waiting for you), the inbox and the mode. `Alt+S` or `F11`
  (`app.status.expand`) temporarily shows every segment, also while a turn runs. Presets
  and custom left/right segments still apply when configured. Learning diagnostics are available in the explicit improvement views.
  The activity label names what the model is doing (**Reasoning**, **Writing**, **Preparing** *tool*),
  and a provider retry reads **retrying…** rather than counting down to zero; once a retried request
  succeeds, one **Recovered** line closes the retry note. A tool that has run for 10 seconds shows its
  elapsed time (**running bash · 1m 15s**). The terminal tab title
  uses a blue circle when Dial is waiting for you.
- The default status line leads with the approval mode as a colored dot and plain label
  (`● Ask first`, `Edit freely`, `Full auto`, `Read-only`): green for ask and read-only, orange for
  full auto. Config, CLI and `/approval` values stay `ask`, `write`, `auto` and `readonly`.
- **Work panel.** The plan lives above the composer, not in repeated transcript cards. Successful
  plan calls are hidden from the transcript; failures remain visible and session history retains the calls.
  While a plan has open steps on a terminal at least 90 columns wide, one compact rounded card
  shows a `Plan` header with a progress bar and completed/total count. It is as tall as its content
  and at most 100 columns wide. Its tab strip shows `plan`, `evidence` and `mode`, switched with `Ctrl+X` (or `Alt+W` or
  `F12`; rebind `app.work.tab`). The plan tab lists the steps with status dots (a green check for
  done, an orange dot for the step in progress, a hollow ring for pending) and the focused step's
  recorded reason or finding; when the card is short it keeps the focused step and drops reason
  lines first. Evidence lists references recorded so far, newest and focused first with a `+N more`
  count for the rest, labelled as agent observations, not verification. Mode shows the approval mode
  and sandbox. At 60–89 columns a collapsed plan-only
  card shows the current and next step; narrower terminals keep the one-line focus summary. The panel
  only reads the plan; it never changes it.
- The launch input is a rounded box that asks "What are we building?". It shows no status line
  or shortcut catalog. An active tab in any panel's tab strip is drawn as a filled pill.
- Skill tool lines include the bundled reference-file path when one is read, for example
  `rust-best-practices · references/handbook-11.md`, so separate reference reads do not look like
  repeated loads of the core skill.
- The `cost` segment shows the observed API cost of the running turn and the session, such as
  `$0.04 turn · $1.20 session`. Amounts show cents from a cent upward and four decimals below
  one (`$0.0041`). While a turn runs, the segment counts spend observed since the turn
  started, including workers and background work that settles meanwhile. When the turn ends it
  shows that turn's exact ledger figure until the next turn starts. The session total includes
  workers and is omitted when it equals the turn. The segment is hidden when the cost is unknown
  or zero: local and subscription models, unpriced requests, and a turn with no priced reply. It
  is part of the default, `nerd`, `ascii`, `compact` and `full` presets; custom layouts add
  `cost` to `ui.status_left` or `ui.status_right`. It is the first segment to drop on a narrow
  line after the token, rate, learning, usage, session and time segments. `/cost` keeps the full
  breakdown.
- Local inference shows **loading model**, **processing prompt**, then a live **generating tokens**
  counter and tokens/second in the composer; preparing a local model also shows as activity.
  Dial's MLX runners report engine counts, including
  reasoning and tool tokens; the text runner also reports prompt processing progress. Other local
  servers use estimates marked `~` until reported usage arrives. Counters reset for each request.
  Text-embedded tool syntax stays hidden from the transcript, but its streamed bytes still advance
  estimated counts and reset the generation-silence timer. Engine-reported counts remain authoritative.
- Built-in guidance asks for brief user-facing findings, decisions, changed plans and blockers,
  even alongside tools, without narrating each routine call. This is a model instruction, not a
  synthetic progress message or a guarantee of prose on every reply. Reasoning is shown only when
  the provider emits it; tool-only replies do not imply that hidden reasoning was received.
- Active work has a dedicated row above the composer: the current operation, elapsed **turn** time,
  and a compact activity track. Reported prompt counts fill the track; unknown progress uses a
  travelling marker, not a guessed percentage. The metadata border stays stable and the row
  disappears when idle. Custom status layouts opt in with the `spinner` segment.
  Loading, prompt processing and generation all report **no progress for Ns** after 15 seconds
  without a progress event. This is a telemetry-silence warning, not proof that inference has hung;
  local first-event waits remain unlimited by default. Stopping takes precedence over old telemetry.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.

- A frameless default look (`dial-dark`/`dial`): shaded bands for your messages and the composer,
  a shared icon gutter for answers and tool calls, and inset panels and menus with a filled
  selection. Dark, light, monochrome, Gruvbox, Nord and Paper themes; custom JSON themes, symbol
  presets, band, box or line composer, styled headings/code/diffs, expandable tool results, and
  `NO_COLOR`.
- Activity reads apart at a glance. Commands, code cells, edits and web calls are framed cards
  with the exit code and time on the bottom edge; shell and code cards are framed from the start
  and show a live elapsed time after one second. A failed command shows its last output lines, and
  hidden-line counts refer to source lines. Denied and stopped calls show a muted `⊘ denied` or
  `⊘ stopped` rather than a failure. Runs of reads and searches fold into `Read (N)` /
  `Search (N)` trees. Notes sent to other agents show as quoted `✉ IRC` lines with delivery
  receipts. Skill and tool loading stays in the quiet `◆` context lane. Collapsed reasoning reads
  `◇ reasoning · N lines (Ctrl+T shows)`. Markdown list bullets render as `•`, code blocks with a
  `│` gutter and task lists as `☑`/`☐`.
- Durations read `45s`, `12m34s` or `1h15m`, and large counts are shortened (`1.5k`).
- Unicode-aware multiline editing, bracketed paste, persistent per-project prompt history and history
  search. `Up`/`Down` (or `Ctrl+P`/`Ctrl+N`) first step through queued messages, then prompt history.
  Secret provider inputs are masked and never enter prompt history.
- The newline hint shows `Shift+Enter` only when the terminal speaks the kitty keyboard protocol;
  otherwise Shift+Enter arrives as plain Enter, so the hint shows `Ctrl+J`.
- Pasted text stays in the draft. Buffered unbracketed paste preserves line breaks, and an immediate
  trailing Enter after bracketed paste is ignored. Press Enter after the paste settles to send it.
  A large paste collapses into a placeholder that Backspace or Delete removes as one unit.
- **Clipboard images:** the paste-image key (`app.clipboard.pasteImage`; `Ctrl+V`, `Alt+V` on Windows)
  attaches the clipboard image as an `[Image #n]` chip in the draft. Sources by platform: macOS reads the pasteboard (PNG, TIFF,
  JPEG, including screenshots) and then an image file copied in Finder; Windows and WSL read the
  Windows clipboard through `powershell.exe`; Wayland uses `wl-paste` and X11 uses `xclip`. Termux
  has no clipboard image API (`termux-clipboard-get` is text only) and reports that explicitly.
  Every image is normalized to PNG and downscaled to fit the 5 MiB attachment limit. If nothing
  usable is found, the error lists each reader that was tried and why it produced no image.
- Questions and approvals quarantine input queued before the dialog opens, and ignore answer keys
  for about 350 ms after the prompt appears, so a buffered Enter or letter cannot answer an unseen
  prompt. Navigation and cancellation work immediately. A question opened over a nonempty draft starts with draft
  focus: Enter edits that draft, Tab/Escape moves to the choices, and Esc or Ctrl+C cancels the question.
  If a question or approval arrives while a full-screen menu is open during a turn, the menu
  temporarily yields to the inline prompt. Its state is kept and it reopens after the prompt;
  menu navigation and any composer draft are preserved.
- In question dialogs, Enter in **Other/custom input** submits the answer (or advances to the next
  question). Shift+Enter (or Ctrl+J) inserts a newline. Enter in **Note** saves the note and returns to the
  choices without submitting. Esc cancels the dialog. The attach-image key (`Ctrl+V`; `Alt+V` on
  Windows) attaches an image path; `Ctrl+A` stays line start.
- **Inline suggestions:** command arguments offer valid choices as dim ghost text even before you have
  used them: `/understanding ` suggests `architecture`, `/skills s` suggests `sources`, and
  `/skills enable ` suggests an installed skill name (including disabled skills). Built-in choices
  take priority over matching history; ordinary prompts still suggest the newest matching history entry.
  `Tab` or `Right` accepts the ghost; `Esc` hides it; `Enter` sends only what you typed. Suggestions
  appear only at the end of an idle draft, never in secret prompts or mid-line.

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

## Context decisions and manual compaction

`/context` separates the **last sent request** from an **estimated next request** for the
selected model, including queued input and the current non-command draft. It shows the usable
prompt budget, response reserve and effective automatic-compaction trigger, rather than treating
the whole model window as available prompt space. Fixed overhead is an empty-history projection;
the remaining history/input estimate is not a promise of reclaimable tokens. Images, native replay,
attachment expansion, request-time guidance and routing can change the actual request.

`/compact [method] [focus]` first previews the selected/fallback method order and the eligible
older history versus recent tail. Summary methods may make local or hosted requests, including
fallback attempts; exact cost and savings are unknown. Hooks and execution-time checks can refuse
the operation or leave history unchanged. Canceling the preview does not start inference or change
history. Pending queue entries and the composer draft are not compacted. If fixed instructions,
context or tool overhead exhaust the prompt budget, reduce that overhead or select a larger-window
model with `/model`; compacting history alone cannot solve that capacity problem.

## Attachment preflight

Drafts containing `@path`, `@diff`, `@staged` or pasted image paths receive a bounded background
preview using the same expansion rules as submission. The composer summarizes included, truncated
and skipped attachments. Text-token counts are estimates; image tokens vary by provider. Pending
previews keep the draft in place. Missing, unreadable, truncated or otherwise skipped input requires
a second deliberate submission; holding Enter does not acknowledge the warning.

Mentions inside Markdown code (fenced blocks and inline backticks) are left as written. An
unresolved `@name` warns only when it looks like a project file: its first directory exists, or it
ends in a short file extension (`@notes.md`). Scoped packages and decorators such as
`@angular/core`, `@types/node` and `@pytest.fixture` pass through as text without a warning.

To inspect an existing draft without replacing it, open the command palette and choose
**attachments**. Select a reference, then **Keep** (default) or **Remove reference**. Removal edits
only the mention in the draft, never the underlying file. `/attachments inspect` prints the cached
report and `/attachments remove <number>` removes an indexed mention when invoked with a retained
draft. These previews are not frozen send payloads: files are read again at submission and can
change between the preview and the actual request.

During a running turn `/context` labels the last observed request separately and explicitly says
that a fresh next-request projection is unavailable. Run it after the turn for the selected-model
budget and recommendations. Use the command palette to inspect context or compact while preserving
an existing draft.

## Recovering unsent input

Dial periodically checkpoints the draft, collapsed paste payloads, queued messages and their
routes/order in private project/session-bound recovery state. Clipboard image payloads use
integrity-checked private sidecar files; ordinary project attachments remain file references.
This is crash recovery, not a secure vault or a guarantee that the very latest keystroke survived
a sudden crash. Sensitive pasted material is stored locally until removed; no secure-erasure claim
is made, and unreferenced image sidecars may remain after a saved record is discarded.

When saved input is found, Dial offers `/draft restore` and `/draft discard`. `/draft` (also in
the palette) opens explicit **Keep working**, **Restore**, **Discard saved** and **Release held
queue** choices. Restore refuses to overwrite an existing draft/queue. `/draft replace` asks for
confirmation before replacing current input. `/draft status` reports saved records.

Restoration never submits a prompt: restored queues stay held until you deliberately send one or
use `/draft resume`. Discard removes saved records, not the current editor or queue. Conversation
switches first preserve their own input; a save failure keeps the original editor and blocks the
switch. Recovery is bounded (four retained records, 2 MiB text per snapshot and an 8 MiB serialized
record cap, with bounded image sidecars). Capacity, corrupt-record and concurrent-writer errors
are shown rather than silently dropping saved input. Use the explicit saved-record discard action
or reduce input to resolve capacity errors.
