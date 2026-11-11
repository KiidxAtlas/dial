# Keyboard shortcuts

Find the active keys, edit bindings and handle terminal-specific keyboard differences.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

| Action | Shortcut / command |
|---|---|
| Search commands and actions | `Alt-K` or `F1`, `/help` |
| Model selector / roles | `F2` on macOS (`Alt-M` elsewhere; both work everywhere) / `Alt-R` or `F3`, `/model` |
| Provider setup | `Alt-P` or `F4`, `/providers` |
| Settings | `Ctrl-,` (`Ctrl-4` fallback), `/settings` |
| Edit keyboard shortcuts | `/hotkeys`, or `/settings` → Setup → Keyboard shortcuts |
| Previous/next prompt | `Up` / `Down` or `Ctrl-P` / `Ctrl-N`; steps through queued messages first, then history, and preserves the unsent draft |
| Search prompt history | `Ctrl-R` |
| Command list / completion | `/`, then `Up` / `Down`, `Tab` or `Right` to complete, `Esc` to dismiss |
| Accept inline suggestion | `Tab` or `Right` at the end of the draft |
| Multiline input | `Shift-Enter` when the terminal supports the kitty keyboard protocol, otherwise `Ctrl-J` (the newline hint shows `Ctrl-J` when Shift+Enter would arrive as plain Enter) |
| Queue a follow-up while busy | `Ctrl-Q`, or `Option+Enter` on macOS / `Alt+Enter` elsewhere (Windows Terminal keeps `Alt+Enter` for fullscreen) |
| Expand/collapse tool results / show or hide tools | `Ctrl-O` / `Alt-O` or `F5` |
| Worker hub / latest worker | `Alt-A` or `F6` / `Alt-Shift-A` or `F7` |
| Next favorite model | `Alt-]` or `F8` |
| Repaint the screen | `Ctrl-L` or `Alt-L` |
| Attach the clipboard image / dictate | `Ctrl-V` / `Alt-V` or `F9`; on Windows `Alt-V` attaches the image and `F9` dictates |
| Scroll transcript | `PageUp` / `PageDown` |
| Close a panel or chooser / stop the turn | `Esc` / `Ctrl-C` (while busy with a draft, the first `Ctrl-C` clears the draft and a second stops; the hint then offers `Ctrl+-` to restore the draft. In any panel, idle or busy, `Esc` steps back one level and `Ctrl-C` closes the panels without stopping the turn; the worker viewer keeps `Ctrl-C` for cancelling its worker. During a turn the panel's bottom row says so) |
| Quit from an empty composer | `Ctrl-D` twice (the first press shows "again to exit"), `/exit` |
| Expand the status line | `Alt-S` (`F11`) |
| Cycle approval mode | `Shift-Tab` cycles ask → write → read-only; full auto is chosen only with `/approval auto` (from auto, `Shift-Tab` returns to ask) |
| Show or hide reasoning | `Ctrl-T` |
| Switch the work panel tab (plan, evidence, mode) | `Ctrl-X`, `Alt-W` or `F12` |
| Focus session controls | `F10` |
| Edit the draft in `$VISUAL`/`$EDITOR` | `Ctrl-G` |
| Rate the last turn good / bad | `Alt-Up` / `Alt-Down` |

Hints and footers show only the primary shortcut; `/hotkeys` lists every bound alternative.
`Ctrl-P` and `Ctrl-N` move the selection in every list and browse prompt history in the composer, so
the command palette uses `Alt-K` and `F1` instead. `Ctrl-K` is cut-to-line-end on every platform.
When an application shortcut takes a chord an editing action also uses (for example a custom
`"app.model.select" = "Ctrl+K"`), `/hotkeys` and the shortcuts panel flag the editing chord as shadowed.

Every Option/Alt shortcut also has a Control or function-key default, because macOS terminals type a
symbol for Option+letter (Option+M types `µ`) unless Option sends Meta/Esc (iTerm2: Profiles → Keys →
Left Option key → Esc+; Terminal.app: **Use Option as Meta key**). On macOS, **F2** opens the
model selector and Option+M remains available; bind `Ctrl+K` yourself if you prefer it. Function keys on Mac keyboards may need
`Fn` unless "Use F1, F2, etc. keys as standard function keys" is on. On Windows, Windows Terminal
keeps `Alt+Enter` (fullscreen), `Ctrl+V` (its text paste) and `Ctrl+,` (its settings), so use `Ctrl+Q`,
`Alt+V` and `Ctrl+4` there; `Ctrl+G` opens the draft through `pwsh`, falling back to Windows PowerShell.
Panel keys (`F2`–`F8` in `/models`) are panel-local; in the HTTP header editor `Ctrl+O` adds a row
(`Ctrl+P`/`Ctrl+N` move between rows) and `Ctrl+S` saves, since
GNOME Terminal takes `F10`. A key you bind to an editing action is not shadowed by an application
default on the same key. `/model` works on every platform, and the footer and `/hotkeys` show your
active bindings, including custom overrides.
Shortcut labels use **Option** on macOS and **Alt** elsewhere. **Ctrl** always means Control,
not Command. Both `Alt` and `Option` are accepted in keybinding configuration on every platform;
saved bindings retain the portable `Alt` spelling. `/hotkeys` shows the effective native labels.

### Shift+Enter compatibility

Dial requests enhanced keyboard reporting even if its capability probe fails, so a missed reply
does not disable Shift+Enter. Key-release-dependent features still require a successful probe.
The fallback hint can therefore say `Ctrl-J` even when Shift+Enter works.

If the terminal sends the same bytes for Enter and Shift+Enter, an application cannot distinguish
them. Configure **Shift+Enter to send line feed (`0x0A`)** in that terminal; Dial already accepts
that signal as a newline, and ordinary Enter continues to send the message. This is a terminal
mapping, not a requirement to press Ctrl+J:

- **iTerm2:** Settings → Profiles → Keys → Key Mappings; add Shift+Return with **Send Hex Code**
  set to `0x0a`.
- **Apple Terminal:** Settings → Profiles → Keyboard; add Shift+Return with **Send text** set to
  a literal line feed (press Ctrl+J in the text field).
- **VS Code integrated terminal:** add `{ "key": "shift+enter", "command":
  "workbench.action.terminal.sendSequence", "args": { "text": "\u000a" }, "when":
  "terminalFocus" }` to the keyboard shortcuts JSON.

Dial does not change terminal preferences automatically. Multiplexers must also pass the enhanced
keyboard protocol through, or use the line-feed mapping above.

`/hotkeys` opens the same searchable shortcut editor while idle, during a turn or while waiting for compute.
Type to filter by action, group or key; `Enter` edits, and `Esc` goes back. `Delete` restores the
selected action's defaults only when the cursor is at the end of the search; elsewhere it edits the
search text. While editing, enter chord names separated by spaces, such as `Alt+M Ctrl+K`, or press
`Tab` and then the key itself to record it (`Esc` leaves recording); an empty field disables the action.
Changes save globally to the `[keys]` table of `~/.dial/config.toml` and apply immediately, only after
validation and a successful write. There is no project override. Conflicting bindings are rejected,
and the editor keeps a way to confirm and close panels. You can also edit the `[keys]` table directly;
a legacy `~/.dial/keybindings.toml` is still read as a per-binding fallback. For example:

```toml
[keys]
"app.model.select" = "Ctrl+K"
"app.history.search" = []
```
