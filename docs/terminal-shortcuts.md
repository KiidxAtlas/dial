# Keyboard shortcuts

Find the active keys, edit bindings and handle terminal-specific keyboard differences.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

| Action | Shortcut / command |
|---|---|
| Search commands and actions | `Ctrl-P`, `/help` |
| Model selector / roles | `Ctrl-K` on macOS (`Alt-M` elsewhere) or `F2` / `Alt-R` or `F3`, `/model` |
| Provider setup | `Alt-P` or `F4`, `/providers` |
| Settings | `Ctrl-,` (`Ctrl-4` fallback), `/settings` |
| Edit keyboard shortcuts | `/hotkeys`, or `/settings` → Setup → Keyboard shortcuts |
| Previous/next prompt | `Up` / `Down`; preserves the unsent draft |
| Search prompt history | `Ctrl-R` |
| Command list / completion | `/`, then `Up` / `Down`, `Tab` or `Right` to complete, `Esc` to dismiss |
| Accept inline suggestion | `Tab` or `Right` at the end of the draft |
| Multiline input | `Shift-Enter` when supported by the terminal, or `Ctrl-J` |
| Queue a follow-up while busy | `Ctrl-Q`, or `Option+Enter` on macOS / `Alt+Enter` elsewhere (Windows Terminal keeps `Alt+Enter` for fullscreen) |
| Expand/collapse tool results / show or hide tools | `Ctrl-O` / `Alt-O` or `F5` |
| Worker hub / latest worker | `Alt-A` or `F6` / `Alt-Shift-A` or `F7` |
| Next favorite model | `Alt-]` or `F8` |
| Repaint the screen | `Ctrl-L` or `Alt-L` |
| Attach the clipboard image / dictate | `Ctrl-V` / `Alt-V` or `F9`; on Windows `Alt-V` attaches the image and `F9` dictates |
| Scroll transcript | `PageUp` / `PageDown` |
| Close a panel or chooser / stop the turn | `Esc` / `Ctrl-C` |
| Quit from an empty composer | `Ctrl-D`, `/exit` |
| Expand the status line | `Alt-S` (`F11`) |

Hints and footers show only the primary shortcut; `/hotkeys` lists every bound alternative.

Every Option/Alt shortcut also has a Control or function-key default, because macOS terminals type a
symbol for Option+letter (Option+M types `µ`) unless Option sends Meta/Esc (iTerm2: Profiles → Keys →
Left Option key → Esc+; Terminal.app: **Use Option as Meta key**). On macOS, **Control+K** opens the
model selector (not Command+K) and Option+M remains available. Function keys on Mac keyboards may need
`Fn` unless "Use F1, F2, etc. keys as standard function keys" is on. On Windows, Windows Terminal
keeps `Alt+Enter` (fullscreen), `Ctrl+V` (its text paste) and `Ctrl+,` (its settings), so use `Ctrl+Q`,
`Alt+V` and `Ctrl+4` there; `Ctrl+G` opens the draft through `pwsh`, falling back to Windows PowerShell.
Panel keys (`F2`–`F8` in `/models`) are panel-local; in the HTTP header editor `Ctrl+S` saves, since
GNOME Terminal takes `F10`. A key you bind to an editing action is not shadowed by an application
default on the same key. `/model` works on every platform, and the footer and `/hotkeys` show your
active bindings, including custom overrides.
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
