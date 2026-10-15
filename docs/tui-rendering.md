# TUI rendering

Dial renders transcript blocks into styled ratatui rows; `src/ui/ansi.rs` contains the sanitized terminal-output emulator, `src/ui/syntax.rs` owns bundled syntax grammar mapping, and `src/ui/diff.rs` provides reusable line-numbered and word-level diff spans.

Assistant prose freezes complete top-level markdown blocks and reparses only the mutable tail. Link-reference definitions are retroactive and deliberately trigger a full render. Constructs containing `[` or `<` in the mutable tail remain tail-local, rather than disabling incremental parsing for the whole message. Thinking blocks use the same active/settled lifecycle, but settled blocks are not evicted as transcript cursors advance: each block keeps its last rendered rows until the transcript is dropped. Width/theme changes invalidate the parsing key.

Streaming content may spill stable rows to scrollback while keeping the live tail visible. Resizing replays committed transcript content at the new width; terminal frame diffing reuses unchanged rows. Ctrl+O expands or collapses tool evidence and repaints existing scrollback, including successful reads, searches and checks. Compact diffs honor `ui.tool_output_lines`; expanded cards reveal complete stored output and clipped targets. Worker cards use typed results and show a short finding when collapsed and the full report when expanded. Themes are embedded JSON palettes and selectable by name.

Defaults collapse reasoning, omit per-turn statistics and leave the workspace-change rail off. The worker rail shows active workers only; completed results stay in their task cards and the worker panel. Cache accounting and routine decision advice stay out of the transcript; `/cost`, `/policy` and `/laya status` retain the details. Explicit saved display preferences continue to apply.

Ctrl+P and the empty slash menu show everyday actions. The Work category groups `/workers` and `/processes`. Compute and learning controls remain searchable advanced actions. Advanced modes and diagnostics remain callable and searchable in the Advanced category. `/agents`, `/hub` and `/jobs` open the same worker panel as `/workers`; `/jobs <id>` still shows a job's status and `/agents definitions` lists definitions. `/bg` remains an alias for the process panel.

Optional capabilities are configured through `/setup` on demand; fresh launches do not interrupt chat with an optional setup dialog. A task card stays live until its owned workers finish. Text-only interfaces retain worker start/end notices, while the TUI uses the shared card. Native images reserve a separator cell so tighter transcript spacing does not drop their graphics payload.

Run the UI test suite with `cargo test ui::` from the repository root. Rendering code is pure where possible, so component behavior can be exercised with the ratatui test backend.

Tool definitions own portable `render_call` and `render_result` hooks. The controller resolves these hooks before presentation; the terminal only lays out the label, target, tone, body style, facts and optional compact preview. Expansion always reveals the full result. Automatic checks carry explicit metadata; a command or summary beginning with “Verify” does not change its meaning. Imported sessions use a controller compatibility projection; new journal nodes retain call IDs and presentation for replay. Worker transcript views use the same metadata and retain full evidence.

Tools, turns and workers use `runtime::Phase`: running, completed, blocked, needs_input, incomplete, failed and interrupted. Incomplete workers show a warning and their reason, cancellation remains distinguishable from execution failure, and late results cannot restart a settled spinner. Legacy journal `done` and `cancelled` values still deserialize; the task-tool wire format retains its compatible spellings.

Ordinary settings and model views omit learning controls. `/improve settings` and `/improve models` open dedicated learning views. The default coding status line never polls a learning database or displays training state.
