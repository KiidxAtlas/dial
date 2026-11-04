# Commands and help

Find actions through the goal-first palette, slash completion and command help.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

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
  marked; `Enter` applies it and `Esc` cancels. `/queue` rows edit on `Enter` and remove on a second
  press of `Delete` or `x` (on an empty search) on the same row; the first press names the item, any
  other key cancels, and `Backspace` only edits the search. The argument forms still work in scripts
  and text output.
  `/help` opens the goal-first palette; `/help text` prints the command list and `/help all` includes
  the advanced sections. The footer lists the keys the current mode uses; Esc clears before going back.

- **Slash commands:** typing `/` opens a list of every command (built-in and your `.dial/commands/*.md`)
  that filters as you type. `Up`/`Down` move (wrapping, with a position counter for long lists), `Tab`/`Right`
  complete, `Esc` dismisses without losing the draft. Without navigating, `Enter` sends exactly what you
  typed, so a full command runs at once; after you move the highlight with `Up`/`Down`, `Enter` accepts and
  runs the highlighted command. File mentions (`@path`) complete the highlighted file on `Enter`. Built-ins
  win over a custom command with the same name. `/clear`, `/resume`, `/palette`, `/commands`, `/tree`,
  `/branch`, `/rollback` and `/review-file` still work but are hidden from menus in favor of `/new`,
  `/sessions`, `/help`, `/help commands`, `/sessions this|branch`, `/undo files` and `/review files`.
- **Unavailable commands:** commands that need a Git repository, an active session or an idle
  session stay searchable in completion and the palette but are dimmed with the missing
  prerequisite; running one prints the reason instead of failing later. Availability comes from
  cached state, so typing never probes the filesystem.
- **CLI equivalents:** `/help <command>` (or any alias) explains the command and shows its matching
  `dial …` entry point, for example `/help rename` shows `also: dial sessions rename`.

## Hubs and panels

Related actions share one entry point. Every older spelling still runs; redundant spellings are
hidden from menus.

- **`/mode`** picks plan mode, approval, vibe or skillful mode. `/plan` and `/approval` stay listed.
- **`/dirs`** adds, removes or lists workspace directories (`/add-dir`, `/remove-dir` still work).
- **`/collab`** hosts, joins, leaves, stops or lists live collaboration rooms (`/join`, `/leave`).
- **`/learn`** opens the learning and self-improvement actions (`/improve` still works).
- **`/share`** offers an encrypted link, a transcript export, a dump, or **Copy last answer**
  (`/export`, `/copy`, `/dump` still work). `/trace` stays separate.
- **`/git`** opens Status, Diff, Commit and Log tabs; `Tab`/`Shift-Tab` switch tabs and keep the
  commit draft. `/git tab status|diff|commit|log` opens a tab directly, `/git <revision>` still
  shows that revision (even one named `status`), and `/git commit run [flags]` runs `/commit`.
- **`/activity`** opens Workers, Processes, Compute, Schedule and Queue tabs. `/workers`,
  `/processes`, `/compute`, `/schedule` and `/queue` open their tab. `Ctrl-Tab` reaches a
  panel's inner tabs; attached terminals keep normal `Tab` input.
- **`/extensions`** browses MCP servers, skills, tool groups and hooks; `/extensions
  mcp|skills|tools|hooks` opens a tab and `/hooks` opens the Hooks tab.
- **`/stats`** opens searchable Usage / cost, Tokens, Sessions and Tools tabs across sessions.
  `/stats <range>` and `/stats --port <port>` behave as before; `/usage` (provider quotas) and
  `/cost` (this runtime's accounting) stay separate.
- **`/inbox`** lists this session's notifications; selecting one marks it read. **`/doctor`** lists
  local diagnostics and reviews a fix before applying it. **`/accounts`** lists saved logins
  without showing credentials. **`/set <key> [value] [--project]`** shows a setting and its source
  or saves a validated value.

## Topic help

Use `/help <topic>` to print a documentation path and a short introduction. Topics include
`composer`, `command-reference`, `shortcuts`, `sessions`, `models`, `settings`, `turns`,
`terminal`, `everyday`, `tools` and `setup`. `/help commands` keeps its existing custom-command
list; `/help text` and `/help all` keep the plain-text built-in command lists. Topic help
never opens an external program or starts a model request.
