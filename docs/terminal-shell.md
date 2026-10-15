# Terminal shell

The interactive shell enters raw mode and retains a live inline transcript. Full-screen panels use `InlineTerminal::open_overlay`, `draw_overlay` and `close_overlay`, which preserve the inline screen while a panel is open; `with_overlay` is the scoped adapter for legacy blocking callers. External programs temporarily restore cooked terminal state through `suspend`.

Terminal output is written through the inline terminal backend. Bracketed paste is enabled, terminal resize forces a repaint/reflow, and the overlay backend can report mouse input independently of inline transcript scrolling. `OverlayTerminal` is the scoped drawing surface used by overlay controllers.

Notifications and terminal capability settings are registered under `ui.*` in the settings registry. Ctrl-Z uses the terminal's job-control suspend path on Unix; after the foreground process resumes, the terminal is re-armed for repaint.

External draft editing creates a private temporary file with exclusive creation and no symlink following on Unix, writes the current draft, and removes the file after the editor exits. A running turn should never synchronously launch an editor; defer that action until the shell returns to idle.
