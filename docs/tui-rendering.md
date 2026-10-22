# TUI rendering

Dial renders transcript blocks into styled ratatui rows. Markdown and fenced code become rows in `crates/dial-ui/src/ui/markdown.rs`, which owns a compact lexer for bundled syntax colours (`highlight`; unknown fence languages stay plain) and diff-aware styling for fenced diffs (`is_diff`, `diff_style`). Terminal-safe text sanitization lives in `crates/dial-ui/src/ui/text.rs` (`sanitize`). Tool-result diffs and hashline previews are styled in `crates/dial-ui/src/ui/tool_cards.rs` (`edit_spans`, `diff_rows`). Unified diffs for edit and write results are produced by `crates/dial-tools/src/tools/edit/diff.rs`; the vt100 screen used for background process output is `crates/dial-tools/src/tools/exec/background/table.rs`.

Assistant prose freezes complete top-level markdown blocks and reparses only the mutable tail. Link-reference definitions are retroactive and deliberately trigger a full render. Constructs containing `[` or `<` in the mutable tail remain tail-local, rather than disabling incremental parsing for the whole message. Thinking blocks use the same active/settled lifecycle, but settled blocks are not evicted as transcript cursors advance: each block keeps its last rendered rows until the transcript is dropped. Width/theme changes invalidate the parsing key.

Streaming content may spill stable rows to scrollback while keeping the live tail visible. Resizing replays committed transcript content at the new width; terminal frame diffing reuses unchanged rows. Ctrl+O expands or collapses tool evidence and repaints existing scrollback, including successful reads, searches and checks. Compact diffs honor `ui.tool_output_lines`; expanded cards reveal complete stored output and clipped targets. Worker cards use typed results and show a short finding when collapsed and the full report when expanded. Themes are embedded JSON palettes and selectable by name.

Defaults collapse reasoning, omit per-turn statistics and leave the workspace-change rail off. The worker rail shows active workers only; completed results stay in their task cards and the worker panel. Cache accounting and routine decision advice stay out of the transcript; `/cost`, `/policy` and `/laya status` retain the details. Explicit saved display preferences continue to apply.

Ctrl+P opens goal-first action navigation; typing searches every registered command. The empty slash menu remains a command-oriented shortcut. It opens upward from the input (12 rows by default, `ui.autocomplete_items`); `/` groups everyday commands by category, naming each group once, then lists the rest. Typed queries rank fuzzy matches across all commands. Compute and learning controls stay searchable. The live work summary shows Now, Blocked and Next; `/todo` opens the full read-only plan and recorded evidence. Busy draft submission opens a local intent chooser before any message is routed; empty Enter no longer interrupts queued work without an explicit choice. `/agents`, `/hub` and `/jobs` open the worker panel; `/jobs <id>` still shows status and `/agents definitions` lists definitions. `/bg` remains an alias for the process panel.

Composer and history-search editing use the configured editor bindings, including Unicode-aware
word movement. The worker panel ignores key-release events; holding the cancellation key cannot
confirm its own prompt. Page keys navigate the roster in narrow layouts. Worker search and message
draft backspace remove whole graphemes. Tight pinned summaries put input requests first and keep a
worker visible before its optional live preview; status comes before long names and task text.

Until the first message, a live home sits in the viewport above the composer (`crates/dial-ui/src/ui/welcome.rs`): a block wordmark over a `┈` axis and its reflection, borderless model, project, approval and sandbox facts, and recent sessions or starters, side by side from 110 columns and in one compact column below 24 rows. Shorter terminals shed the reflection, then the large wordmark, then contextual facts before the selected action, and window the list around that action. Down from an empty draft selects a row, Enter resumes the session or loads the starter into the draft for editing, and Esc returns to the draft. When the conversation gets its first user message, the home is committed to scrollback as one summary line.

The default look (the `dial-dark` and `dial` themes) is frameless: user messages and the composer are full-width shaded bands (`userBg`) with a `›` prompt, and the status line sits inside the composer band (`ui.composer_shape = "band"`; `box` and `line` keep a framed composer). Transcript rows share a three-cell icon gutter (`tool_cards::GUTTER`): the answer starts at a `●`, tool calls show a dim `·` once settled, `✗` on failure and `⊘` when interrupted, with a bold verb, a muted target and result bodies hanging off `└`. Panels and menus are inset surfaces on the optional `panelBg` token (`chrome::framed_lines`, `chrome::panel_block`) with a bold title row and a right-aligned italic hint; the selected row is filled with `selectedBg` and drawn in one bold body color (`chrome::select_spans`), with search matches underlined. Other built-in themes keep their colors and take the same layout.

MCP calls are the exception to the frameless transcript: a connected-service-colored outline and
`panelBg` surface identify the service boundary, with an `MCP · server` title and the service-local
action inside. Status glyphs, errors, progress and Ctrl+O evidence expansion keep the same behavior
as native calls. ASCII themes use `+`, `-` and `|` borders; monochrome themes retain the label and
outline even when fills are disabled. Native tools and schema discovery keep their existing rows.

Motion is limited to the start-up wordmark sweep (`fx::SWEEP`, 0.7 s) and a 140 ms reveal when any full-screen panel opens; both follow `ui.motion` and are off under `NO_COLOR`, ASCII glyphs and non-truecolor terminals (`fx::motion`). Panels share one footer vocabulary, documented on `chrome::key_hint_spans`, and one tab style, `chrome::tab_style`.

Optional capabilities are configured through `/setup` on demand; fresh launches do not interrupt chat with an optional setup dialog. A task card stays live until its owned workers finish. Text-only interfaces retain worker start/end notices, while the TUI uses the shared card. Native images reserve a separator cell so tighter transcript spacing does not drop their graphics payload.

Run the UI test suite with `cargo test --workspace --lib ui::` from the repository root (the UI lives in `crates/dial-ui`). Rendering code is pure where possible, so component behavior can be exercised with the ratatui test backend.

Tool definitions own portable `render_call` and `render_result` hooks. The controller resolves these hooks before presentation; the terminal only lays out the label, target, tone, body style, facts and optional compact preview. Expansion always reveals the full result. Automatic checks carry explicit metadata; a command or summary beginning with “Verify” does not change its meaning. Imported sessions use a controller compatibility projection; new journal nodes retain call IDs and presentation for replay. Worker transcript views use the same metadata and retain full evidence.

Tools, turns and workers use `runtime::Phase`: running, completed, blocked, needs_input, incomplete, failed and interrupted. Incomplete workers show a warning and their reason, cancellation remains distinguishable from execution failure, and late results cannot restart a settled spinner. Legacy journal `done` and `cancelled` values still deserialize; the task-tool wire format retains its compatible spellings.

Ordinary settings and model views omit learning controls. `/improve settings` and `/improve models` open dedicated learning views. The default coding status line never polls a learning database or displays training state.

## Visual review sheet

`cargo run --locked -p dial-ui --example ux_gallery -- after` writes
`$TMPDIR/dial-ux-review/after.html` from real ratatui `TestBackend` cells, with synthetic data
and no model requests. Run with `before` **before editing** to capture a comparison; it does not
reconstruct historical code. Serve that directory locally and inspect it in a browser.

The sheet covers home and setup risks, short selections, working input, transcript evidence,
settings and filtered choices, action search, empty sessions, worker rosters before/after paging
and tight worker summaries in dark/light, 60-column and 38-column ASCII layouts. Browser fonts only
approximate terminal typography; it does not verify native input delivery, screen-reader support,
terminal emulators or real provider interactions. Regression tests cover selection, key rebinding,
search/reset/cancel, grapheme deletion and zero-size/short layout behavior.
