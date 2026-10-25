# Terminal UX audit and improvement

## Assessment

Dial's existing terminal-native architecture is worth preserving: scrollback remains useful,
commands are discoverable from the palette, settings use validated storage, themes and glyphs
are centralized, and approvals are distinct from ordinary questions. A replacement web-style
shell would sacrifice those strengths without fixing the main usability problems.

The first pass improved **information priority, visible selection, discoverability and feedback**.
The follow-on interaction pass changes navigation and message-routing decisions themselves:
goal-first discovery, explicit send intent and a focused work summary backed by an inspector.
This is an engineering/design audit, not a usability study; no task-completion or user-satisfaction
gains have been measured.

## Coverage and changes

| Journey / finding | Change | Behavior preserved |
| --- | --- | --- |
| Home: isolation was buried behind a long mode sentence | Separate approval and sandbox rows; explicit automatic/no-isolation warning; workspace card heading; missing-model recovery on its own row | Theme tokens, wordmark, recent sessions, setup entry points |
| Home: a selected session/starter could be cut off in a short viewport | Window around the selection and shed contextual facts first; starter hint says it loads for editing | Enter does not send starter text; Esc returns to the draft |
| Working input: an empty field did not explain steering | Busy placeholder names adding direction; active border uses the accent rather than the success color; queue hint names `/queue` | Existing steering, queue submission and remappable bindings |
| Actions: no matches still advertised actions that could do nothing | Explicit search scope and count; no-match recovery and category-switch hint; omit dead selection hints | Search, categories, normal action execution and cancellation |
| Settings: choice navigation did not scale beyond arrow keys | Type-to-filter labels and raw values; visible-match selection maps back to the original value; Esc clears before leaving; clearer override wording | Validation, source layers, save-on-confirm and cancel-without-save |
| Sessions: empty/filter states and unavailable actions were difficult to distinguish | Contextual empty/search states, search-clear-before-close, relevant selected-action hints and short-layout budgeting | Journal ownership checks, explicit delete confirmation, rename/fork/pin behavior |
| Questions: truncation could hide the current option | Window around the choice; honor zero width/height | Existing multi-select, notes, custom input and question protocol |
| Approvals/recovery: tiny budgets could overflow and hide a consequential choice | Pause input confirmation below a 36-column, five-row panel budget (11 terminal rows including shell reserve); resize message; cancellation remains available; shorter explicit session/project scopes | Hotkeys, paging, permission scope and the underlying approval policy |
| Evidence: an unknown turn status was displayed as failure | Unknown status uses a warning; interrupted tools no longer repeat an `interrupted` body sentinel; expand hints use the effective binding | Full evidence expansion, typed errors and interrupted tool state |
| Blocked work: the details entry point was vague | Name `/todo` in the blocked hint | Task state and command behavior |
| CLI help: ~70 options and 35 commands in one flat, arbitrary list; every subcommand repeated the globals first | Task-ordered option sections (shared `dial::cli::help_heading`), everyday commands first, examples and exit codes after the reference, coloured headings; subcommand options precede grouped globals | Every flag, alias, value and default |
| CLI typo: `dial sesions` silently started a full agent turn | A lone lowercase word within one or two edits of a subcommand exits 2 with clap's did-you-mean error and the `dial -- <word>` escape; `-p`/`--print`, multi-word prompts and `--` are untouched | Prompt-as-arguments behavior for real requests |
| CLI listings and diagnostics: bare `dial sessions` lines, `/setup` hints in a shell, dead-end resume errors | Terminal table with header, relative age, fitted titles and next commands (pipes unchanged); `dial doctor` ✓/· marks and `dial doctor --fix` hints, exact `--yes` command, unknown-id recovery; session-id errors name matches and the next command | Script-facing piped output, setup recipes and resume semantics |
| Steering: the steer/queue strip was flush-left lowercase prose unlike other status rows | Indented rows with a stage glyph (○ ● ✓, ASCII-safe), bold label, stage detail and the muted quoted message, shared by steer and queue rows | Stages, queue routing, hints and narrow-width clipping |
| Sessions: pins sorted wherever their date fell until a pin was toggled | Picker opens pins-first (stable partition), so Pinned is one header at the top | Resume/rename/fork/delete indices resolve through the picker's own list |
| Work plan: every row repeated `Phase:` and hid why a step was blocked | Rows (and details) lead with the reason or finding, then the phase | Stable step numbers, priority order, read-only browsing |

## Interaction model

- **Goals before commands.** The action entry point asks what the user wants to do instead of
  leading with a large set of subsystem categories. A goal narrows the existing registry; typing
  remains global search, and slash commands remain the expert path. Back navigation is separate
  from executing an action.
- **Intent before routing.** Busy draft submission asks whether the message changes the current
  task or belongs after it. Opening or cancelling the chooser does not send anything. The direct
  follow-up shortcut remains available; slash commands keep their existing dispatch policy.
  Empty submission no longer stops a turn just because a queue exists: the default is Keep working,
  and Stop and send oldest requires an explicit selection. Editing an existing queue item preserves
  its established position and route.
- **Focus before history.** The live plan uses at most four rows: counts, current step, blockers,
  then upcoming work as space permits. Completed history and full reasons move to `/todo`, a
  searchable, read-only inspector. Step details expose recorded findings and evidence, not fresh
  verification claims. The details panel can page and return to the selected step; Esc leaves
  without changing the plan. `/todo list` retains the plain-text view.

## Visual system

Keep the shared `chrome` vocabulary, semantic theme tokens and glyph fallbacks. The default look
is frameless: hierarchy comes from shaded bands (messages, composer), one inset panel surface
(`panelBg`) for panels and menus, a shared icon gutter and accurate state colors, not from
borders or animation. Approval and isolation are independent home facts rather than a dense
status sentence. Busy is not success. Selected rows remain visually distinct (filled, one bold
body color), and ASCII, light and narrow layouts remain supported.

The offline `ux_gallery` example renders actual ratatui cells using synthetic fixtures. It is a
repeatable way to review the main states without provider requests or exposing local data; see
[tui-rendering.md](tui-rendering.md#visual-review-sheet). Its HTML wrapper is only a review aid,
not a proposed browser UI. Capture `before` before editing; the command cannot reconstruct an
older version. The original baseline covers home, composer, settings and actions; additional
edge-case fixtures were added during the implementation.

## Verification boundaries

Behavioral regressions cover filtered selection/save/cancel, Unicode and no-match handling,
short/zero-size layouts, selected-row visibility, steering and effective shortcut labels.
The interaction pass exercised goal drill-in/back/global search, picker compatibility, busy send
intent and queue retention, repeat-key safety, remapped keys, and plan ordering/filtering/details.
The app library suite passed 346 tests; after the final rendering adjustments the UI suite passed
745 tests with 2 ignored. The cell gallery was regenerated and inspected in the browser at wide,
compact and ASCII widths. Formatting and diff whitespace checks passed. Clippy for both packages
(including examples) completed before the last two rendering-only adjustments; Clippy
still reports existing argument-count warnings, and `dial-computer` emits dead-code warnings.
Rendered screenshots supplement those tests; they cannot prove native terminal input delivery.

No real-model or paid runs are needed for this pass. No claim is made about provider connection
success, download progress under network failure, screen-reader compatibility or every terminal
emulator. Native desktop capture was unavailable, so visual review uses the ratatui-cell gallery
in a browser with approximate terminal typography.

## Remaining product work

These are separate, larger investments, not hidden completion claims:

- **Accessibility and native terminal matrix:** test VoiceOver/screen readers, terminal contrast,
  keyboard reporting and resize behavior in actual emulators. Do not label the TUI accessible
  from source review or browser screenshots alone.
- **Settings density and pointer parity:** section navigation is less discoverable on narrow
  screens and settings remain keyboard-first; consider a common pointer/viewport contract across
  settings and existing mouse-enabled panels, with native interaction testing.
- **Setup and failure journeys:** exercise fresh-provider credentials, expired authentication,
  offline downloads and recovery with controlled service fixtures. They were source-audited, not
  tested against real services here.
- **Status vocabulary:** raw approval identifiers (`ask`, `write`, `auto`, `readonly`) remain
  compatibility values. Home explains them; renaming them throughout configuration, status and
  documentation requires a deliberate product/migration decision.
- **User evidence:** validate discoverability with observed tasks (start work, steer a turn,
  find a setting, resume a session, inspect a failed tool). Add timing and error-rate measurement
  before claiming that the interface is faster or more intuitive for users.

The changes deliberately do not replace command names, weaken approvals, change journal
ownership, start inference to populate the UI, or introduce a second configuration system.
