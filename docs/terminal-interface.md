# Terminal interface

Everyday use of the interactive terminal UI: composer, slash commands, shortcuts, settings panels, model settings, themes and previews. Rendering internals are described in [tui-rendering.md](tui-rendering.md).

## Interface overview and shortcuts

- Inline, scrollback-native transcript with a live composer; finished turns remain in normal terminal
  scrollback. Model, provider and settings panels use a full-screen overlay and restore the terminal
  when closed. Approval prompts and settings adapt to narrow terminal widths.
- A configurable status line shows model, role, path, Git state, context, token usage/rate, approval,
  sandbox and session state. Settings let you choose presets or define left/right segments. Learning diagnostics are available in the explicit improvement views.
- Local inference shows **loading model**, **processing prompt**, then a live **generating tokens**
  counter and tokens/second in the composer. Dial's MLX runners report engine counts, including
  reasoning and tool tokens; the text runner also reports prompt processing progress. Other local
  servers use estimates marked `~` until reported usage arrives. Counters reset for each request.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.
- **Action palette:** `Ctrl-P`, `Alt-K` or `/palette` searches built-ins, custom commands and enabled
  skills without replacing your chat draft. `Tab` / `Shift-Tab` switch categories; `Enter` runs the
  selected action, asking for required arguments first. `Alt-Enter` edits optional arguments.
  During a turn, the panel explains whether the selected action runs immediately, waits or stops
  the turn. Pasting never runs an action. Provider discovery waits until Models or Providers opens.
- Dark, light, monochrome, Gruvbox, Nord and Paper themes; custom JSON themes, symbol presets,
  rounded or line composer, styled headings/code/diffs, expandable tool results, and `NO_COLOR`.
- Unicode-aware multiline editing, bracketed paste, persistent per-project prompt history and history
  search. Secret provider inputs are masked and never enter prompt history.
- Pasted text stays in the draft. Buffered unbracketed paste preserves line breaks, and an immediate
  trailing Enter after bracketed paste is ignored. Press Enter after the paste settles to send it.
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
- While a turn runs, `Enter` steers the current turn: Dial cancels the pending model request or
  discards its stale response before executing tools. A tool already running may finish; unstarted
  actions in its batch are skipped, and completed results are retained. The latest instruction takes
  precedence over conflicting older plans and task summaries. `Option+Enter` on macOS (`Alt+Enter` elsewhere) queues a follow-up for the
  next turn. The queue is visible above the composer, showing up to three messages and a `+N more` line for the rest.
  With an empty draft and queued messages, `Enter` stops the turn and sends the oldest queued message; the hint says so.
  Hints advertise `steer` or `follow-up` only when the draft is not blank. `Up` brings back the last queued
  message for editing (`/queue` edits any item); saving keeps its position and steering/follow-up routing, and
  `Esc` cancels the edit with the original message restored unchanged. Interrupting preserves queued messages and the draft; the status line shows `stopping after <tool>` (or `stopping`) at once, and an interrupted turn does not send a "Turn finished" notification. Slash commands never become
  model steering: `/help`, `/hotkeys`, `/todo`, `/skills`, `/commands` and `/context` run at once
  without stopping the turn. `/model`, `/models`, and the model shortcut (`Ctrl+K` on macOS,
  `Alt+M` elsewhere) open the picker immediately; a selection
  applies after the current turn, without canceling it. Closing the picker without selecting changes
  nothing. A selection changes only this session and the saved default for new sessions; other
  running Dial sessions keep their models. Other commands wait for the turn to finish, with a notice, except commands
  that replace or alter the active session/work: `/exit` (`/quit`, `/q`), `/new`, `/clear`, `/resume`, `/sessions`,
  `/tree`, `/fork`, `/undo`, `/rollback`, `/compact` and `/plan` stop it first.
  `/queue` inspection and queue edits also leave the current turn running. `/model restart` is refused
  while busy without cancelling inference. Live informational notices do not settle or replay the
  streaming answer.
- In `/models`, highlight a model and press `Ctrl-F` to favorite or unfavorite it.
  The **Favorites** view lists saved favorites; `*` marks favorites that are not the current main model.
  Favorites use the picker's global/project scope (`Ctrl-S`) and persist across restarts.
  In All models, favorites are pinned below the current main model and above other models.
  Press `Option+]` on macOS or `Alt+]` elsewhere (remappable as `app.model.favoriteNext`), or use `/model next` to cycle through
  available chat favorites without opening the picker. Unavailable/embedding models are skipped;
  a switch requested during a turn queues until it finishes rather than interrupting it.
  On the first press, if discovery is needed, the switch is retained and applies when ready.
  Escape or `/model cancel` cancels that pending switch. Changes to the selected model, saved
  favorites, scope, or discovery request also cancel it rather than applying a stale choice.
- `/model local` (also `/models local`; offered by slash completion) opens the **local models** dashboard: one row per downloaded MLX or GGUF model (and per local
  model the recorded requests name), with its format, decode speed, memory fit, learning-store version and
  adapter, last LoRA gate, and what improvement is waiting on. Highlight a row to see its details; `Esc` closes.
  The dashboard is a read-only snapshot taken when it opens. It loads no model, starts no Python or MLX process,
  makes no network call, and does not create a stats database or learning store that is missing. The same data is
  printed by `dial models status` (`--json` for the machine-readable form, with identical field names).
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
  followed by recent journal activity rather than creation time. Empty sessions are hidden unless
  pinned; **Tab** includes them. Titles take priority over IDs and model details; the selected
  conversation shows its latest user prompt and assistant answer. Search matches words in any
  order across titles, prompt/answer previews, model and ID, with exact title matches first.
  **Ctrl-U** clears search, **Ctrl-P** pins/unpins without resuming, **Home/End** and
  **PageUp/PageDown** navigate, **Enter** resumes and **Esc** cancels. `/resume <id-prefix>`
  still opens a unique ID directly, including empty sessions. `/tree` keeps fork ordering and
  includes empty sessions initially. The picker keeps up to 200 conversations and 200 empty
  sessions; listing occurs once per open, not per keystroke.
- `/rename <title>` names a session; `/export [path] [--html]` exports Markdown or self-contained
  HTML without overwriting existing files. `/copy` copies the last answer; `Ctrl-G` opens the
  draft in an external editor.
- Resuming displays durable todos and recorded check statuses; these are history, not fresh test
  results. Completed tools are not replayed automatically. `/review` reviews a frozen local or GitHub
  PR scope with source-backed findings; `/review-file` lists checkpoint turns, and
  `/review-file <turn> [path]` previews a turn with **Keep / Revert file / Cancel**.
  A revert includes all changes shown for that file, including your own edits: review carefully.
  Changes after preview are refused, other files are untouched, and the revert has its own `/undo`
  checkpoint. Private, binary or oversized interactive diffs require manual review.
  Comparison and restore are not filesystem-atomic against external writers; avoid editing the
  selected file concurrently with a revert.
- Request status distinguishes waiting, model loading, prompt processing, generation, retry and
  cancellation. Time without a worker progress event is separate from total task elapsed time.
  `/model restart` explicitly confirms unloading only Dial's managed local main model, which reloads
  with the same selection next request. Busy turns, running children and external endpoints are
  refused; no automatic model switch or server kill occurs.
- `/plan [on|off]` enforces read-only planning. The model's `exit_plan` request asks for approval
  before enabling edits. `/permissions` lists scoped rules; for example,
  `/permissions allow --persist bash cargo test` persists a project rule outside the workspace.
  Deny rules take precedence, including in automatic approval mode.
- Idle terminal frames are not rewritten. Fenced code gets theme-aware lexical highlighting,
  and completed Markdown prefixes are cached while streaming. Edit and write cards show the change as a
  numbered, coloured diff (separate hunks with two lines of context, per file); the anchored preview rows
  and removed-lines report the model gets are left out. Each todo call renders the list as it now stands,
  with the steps that call closed struck through and emphasized. Passing automatic checks are one line.

| Action | Shortcut / command |
|---|---|
| Search application actions | `Ctrl-P` / `Alt-K`, `/palette` |
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

`/hotkeys` opens a searchable shortcut editor when idle (during a turn it prints the active reference).
Type to filter by action, group or key; `Enter` edits, `Delete` restores defaults, and `Esc` goes back.
Enter chord names separated by spaces, such as `Alt+M Ctrl+K`; an empty field disables the action.
Changes save globally and apply immediately, only after validation and a successful write. Conflicting
bindings are rejected, and the editor keeps a way to confirm and close panels. You can also edit
`~/.dial/keybindings.toml` directly, for example:

```toml
"app.model.select" = "Ctrl+K"
"app.history.search" = []
```

`/settings` is the configuration hub. Type to search across every category; `Left`/`Right` change
categories and `Tab`/`Shift-Tab` jump sections. `Enter` toggles a switch or opens an editor/choice list;
choices are saved only when confirmed, and `Esc` cancels. `Ctrl-S` switches **All projects** / **This
project** storage; the title shows the destination and successful saves are acknowledged. `Delete`
removes the selected override so the inherited value applies. The detail area identifies the effective
source and restart requirements. The **Setup** category opens models, providers and keyboard controls.

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
their windows, and chat-template arguments (for example `{ enable_thinking = false }`); LM Studio rejects XTC and chat-template arguments. Those request
controls are never sent to cloud providers. Startup options of externally managed servers belong to
those servers, not this page.

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

## Workspace rail and live worker cards

The optional **workspace rail** above the composer shows per-file additions/deletions, including
pre-existing changes; new files are labelled `new` and binary diffs `binary`. It starts off.
`/changes` lists the cached workspace snapshot.
**Pinned workers** sit above the composer and show running and recently finished workers: a header with the active and done counts, up to three rows in collapsed mode, a live line with the latest output (when `ui.worker_live_preview` is on) and an elapsed clock after five seconds. `ui.pinned_workers` selects `collapsed` (default), `full` (more rows) or `off`. The block is hidden on terminals shorter than 16 rows. The `workers` status segment shows `running/total workers`, adding `· $cost` once a worker reports cost; the block, the segment and the hub read the same snapshot. When a worker finishes, needs input or fails while no subagent view is open, one short notice names it and points to `Alt+A`. `Alt+A` opens the subagent hub. `Alt+Shift+A` opens the transcript of the most recently active running worker; press it again to cycle through the running workers. With none running it cycles through all workers, and with none it opens the hub. Double-Left on an empty composer also opens the hub. Pinned rows are not clickable: mouse capture is enabled only while a full-screen panel is open. Rebind `app.subagents.recent` to change the transcript key.
Toggle the block in `/settings` → Appearance → Display. Ctrl-P and the empty slash menu start with everyday
actions; advanced controls remain searchable. `/workers`, `/processes` and `/compute` group work
controls. Ctrl-O expands evidence, including earlier reads, searches, checks and worker reports.
Reasoning starts collapsed and per-turn statistics start hidden. Cache accounting and routine
decision advice stay out of the transcript. Viewed images use compact inline previews when supported
by the terminal; optional capabilities remain available through `/setup` without a startup dialog.
