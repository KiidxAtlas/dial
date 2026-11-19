# Everyday Dial

A short loop: ask, steer, review, and keep what works.

## Start

Run `dial` in your project directory. Type a concrete request and press **Enter**.
Include the outcome you want and any limits: “Fix this failing test without changing the API.”
Use `@` to attach files. Open the command palette (**Alt+K** or **F1**) when you know what you want to do but not its command.

The home screen previews recent conversations when there is room. Press **Down** to focus
one, then **Enter** to resume it; **Up** returns to the input. Safety and setup warnings
stay visible before recent sessions in short windows.

On a first visit, a three-step tour explains the essentials. **Esc** or sending your first
request dismisses it permanently. Its marker is kept in Dial's private user state, not the project.

## Steer

While Dial works, type a correction or another request. Follow the input's busy choices to
steer the current work or queue the request. Be explicit about scope: “Stop after the tests;
don't commit.” To stop the current answer, press **Ctrl+C** at the empty composer. With a panel
open or a draft in the composer, the first press closes the panel or clears the draft instead.

## Review

After an edit, read the compact turn report before accepting the patch. It names changed paths
and shows whether the recorded checks passed, failed, or no longer cover the latest edit.
Unverified means Dial has no verdict it can rely on. **Ctrl+O** expands the evidence and tool
output. Unfinished tasks and effects outside `/undo` remain visible; plain answers stay quiet.

Use `/review uncommitted` for an AI review or `/review files` to inspect checkpointed changes
yourself. Read failures and stale-check notices before treating work as done. A current check is
not necessarily a passing check; inspect the expanded report for the recorded results.

## Undo

Choose what you want to keep before restoring anything:

- **Keep the conversation:** `/undo files` restores files from the most recent checkpoint.
- **Rewind the conversation too:** `/undo` restores checkpointed edits and rewinds the turn.
- **Decide file by file:** `/review files` lets you inspect the changes before choosing.

Shell commands and extensions can have effects outside checkpoints. For example, neither form of
undo stops a service. To capture project files changed by shell commands, see
[`shell.undo_snapshot`](shell.md#undo-for-shell-changes).

## Resume

If you remember the conversation title, use `/sessions` or pick it from the home screen. If you
remember a phrase instead, use `/search <words>`, then `/search` to inspect the matching messages.
Press **Enter** on a match to resume its conversation, or **Esc** to keep working here. After
opening a hit, `/search back` returns to the conversation you started from.
After an interruption or failure, send a new message to continue with the saved session and
its unfinished tasks. Tell Dial what to retry or change rather than starting over unnecessarily.

## Settings and models

Use `/settings` for preferences and the command palette (**Alt+K** or **F1**) to discover actions.
Use `/models` for model selection, roles, downloads, favorites, and filters—all in one panel.
Keep permission and sandbox choices appropriate to the project, and read approval prompts
before allowing risky actions.

## Command line

`dial --help` lists the everyday subcommands: `sessions`, `review`, `commit`, `models`, `config`,
`doctor`, `update` and the rest you use from a shell. Developer and research tools live under
`dial dev`: `benchmark`, `edit-bench`, `improve`, `steering`, `weights`, `laya`, `policy`,
`reliability`, `rules`, `anonymize` and `gc` (`dial dev --help`). The earlier top-level spellings
such as `dial gc` or `dial weights analyze` still run the same commands; they are only hidden from
help and shell completions.
