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
- Active work has a dedicated row above the composer: the current operation, elapsed **turn** time,
  and a compact activity track. Reported prompt counts fill the track; unknown progress uses a
  travelling marker, not a guessed percentage. The metadata border stays stable and the row
  disappears when idle. Custom status layouts opt in with the `spinner` segment.
  Loading, prompt processing and generation all report **no progress for Ns** after 15 seconds
  without a progress event. This is a telemetry-silence warning, not proof that inference has hung;
  local first-event waits remain unlimited by default. Stopping takes precedence over old telemetry.
- Searchable model/role, provider and settings panels. Settings save globally or per project outside
  the editable workspace, validate before writing, and apply display changes immediately.
- **Action palette:** `Ctrl-P`, `Alt-K` or `/palette` searches built-ins, custom commands and enabled
  skills without replacing your chat draft. Everyday actions have task-oriented labels such as
  **Choose a model** and **Set up optional tools**; search accepts these labels or the original
  command names. The selected action still shows its exact slash command and description.
  `Tab` / `Shift-Tab` switch categories; `Enter` chooses an action, asking for required arguments
  first. The footer offers argument editing only when the selected command accepts arguments.
  During a turn, the panel explains whether the selected action runs immediately, waits or stops
  the turn. Pasting never runs an action. Provider discovery waits until Models or Providers opens.
  On short terminals, details yield space before the navigation/cancel footer does.
- **Pickers and `/help`:** run bare, `/approval`, `/effort` (`/reasoning` and `/thinking` too), `/profile`,
  `/theme`, `/queue` and `/permissions remove` open a searchable list in the TUI with the current value
  marked; `Enter` applies it and `Esc` cancels. `/queue` rows edit on `Enter` and remove on `Delete`,
  `Backspace` or `x` (on an empty search). The argument forms still work in scripts and text output.
  `/help` opens this palette on All; `/help text` prints the command list and `/help all` includes the
  advanced sections. The footer lists only the keys the current mode uses, and `Esc` names what it does.
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
  model steering: `/hotkeys`, `/todo`, `/skills`, `/commands`, `/context`, `/search`, `/workers`, `/message`,
  `/cancel`, `/policy`, `/reliability`, and `/improve status|jobs|stop` run at once without stopping the turn.
  `/help` lists the exact "Stops the turn" and "Runs at once" sets, generated from the lists the dispatcher
  uses; `/clear` is in the stop set, and other commands wait for the turn to finish. `/model`, `/models`, and the model shortcut (`Ctrl+K` on macOS,
  `Alt+M` elsewhere) open the picker immediately; a selection
  applies after the current turn, without canceling it. Closing the picker without selecting changes
  nothing. A selection changes only this session and the saved default for new sessions; other
  running Dial sessions keep their models. Other commands wait for the turn to finish, with a notice, except commands
  that replace or alter the active session/work: `/exit` (`/quit`, `/q`), `/new`, `/clear`, `/resume`, `/sessions`,
  `/tree`, `/fork`, `/undo`, `/rollback`, `/compact` and `/plan` stop it first.
  `/queue` inspection and queue edits also leave the current turn running. `/model restart` is refused
  while busy without cancelling inference. Live informational notices do not settle or replay the
  streaming answer.
- In `/models`, highlight a model and press `Ctrl+F` to favorite or unfavorite it.
  The **Favorites** view lists saved favorites; `*` marks favorites that are not the current main model.
  Favorites use the picker's global/project scope (`Ctrl-S`) and persist across restarts.
  In All models, favorites are pinned below the current main model and above other models.
  Press `Option+]` on macOS or `Alt+]` elsewhere (remappable as `app.model.favoriteNext`), or use `/model next` to cycle through
  available chat favorites without opening the picker. Unavailable/embedding models are skipped;
  a switch requested during a turn queues until it finishes rather than interrupting it.
  On the first press, if discovery is needed, the switch is retained and applies when ready.
  Escape or `/model cancel` cancels that pending switch. Changes to the selected model, saved
  favorites, scope, or discovery request also cancel it rather than applying a stale choice.
- `F8` edits a model's HTTP headers for the role being picked (`Roles`, then `Enter`, then `F8`). The editor title names the model and role, and saving assigns only that role. Outside a role pick, `F8` asks you to choose a role first, so browsing never reassigns main. Inside the editor, `F9` inherits the provider's headers (`Tab` only moves between fields, so it works on legacy terminals). Below 56 columns the groups column is hidden and cannot take focus.
- **Get models** is the group after Favorites in `/models` (`/model local` and `/models local` open it directly). It finds and installs local models. Its MLX, GGUF and Embedding shelves switch with `Tab`. Each Hugging Face shelf lists the models already downloaded in that format (highlight one to see its speed, memory fit and waiting improvement; Enter makes it the main model), then search results: type a query and press Enter, or leave the query empty for a curated list. Enter on a result lists its files and downloads it in the background; GGUF asks which quantization first. The Embedding shelf lists the curated Ollama embedding models, marks the ones Ollama already has, and Enter sets one up as `/setup embeddings:<model> yes` does. Esc cancels a running download. Downloads keep running while you browse other groups; closing `/models` cancels them. See [model-downloads.md](model-downloads.md) and [model-embeddings.md](model-embeddings.md).
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
  resumes. A session another Dial process has open is marked `[open elsewhere]`; choosing it keeps
  the picker open with the reason.
  `/resume <id-prefix>` opens a unique ID directly, including empty sessions; an ambiguous prefix
  opens the picker with that prefix as its search. Choosing the current session notes
  `Already in session`. After a resume, the note shows the title, model, last activity and the
  recorded state. `/tree` shows fork depth: each fork is indented under its parent, and siblings are
  ordered most recent first; it includes empty sessions initially. The picker keeps up to 200
  conversations and 200 empty sessions; listing occurs once per open, and each action reloads the
  list rather than running per keystroke.
- `/branch` with no argument opens a list of the nine most recent user messages (keys 1-9);
  `/branch <n>` names one directly. Either way the rewind asks first and says how many messages
  leave the view. `/delete` and `/session delete` ask before removing the current session file;
  cancelling keeps it.
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
  refused; the command never switches models or kills external servers. Failed/cancelled inference
  on a Dial-managed server retires its process before retry so abandoned work cannot block the next request.
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

`/settings` is the configuration hub. Type to search labels, descriptions and configuration paths
across every category; search words can appear in any order. `Left`/`Right` change categories and
`Tab`/`Shift-Tab` jump sections. `Enter` toggles a two-state switch or opens an editor/choice list.
Settings with an automatic mode open an explicit **Auto / On / Off** picker at the current value
instead of cycling and saving immediately. Choices save only when confirmed; `Esc` cancels without
saving. `Ctrl-S` switches **All projects** / **This
project** storage; the title shows the destination and successful saves are acknowledged. `Delete`
removes the selected override so the inherited value applies. The detail area identifies the effective
source and restart requirements. The **Setup** category opens models, providers and keyboard controls.

While you search, the category tabs are not offered and `Tab` does nothing; `Esc` clears the search first.

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
