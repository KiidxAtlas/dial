# Everyday Dial

A short loop: ask, steer, review, and keep what works.

## Start

Run `dial` in your project directory. Type a concrete request and press **Enter**.
Include the outcome you want and any limits: “Fix this failing test without changing the API.”
Use `@` to attach files. Open **Ctrl+P** when you know what you want to do but not its command.

The home screen previews recent conversations when there is room. Press **Down** to focus
one, then **Enter** to resume it; **Up** returns to the input. Safety and setup warnings
stay visible before recent sessions in short windows.

On a first visit, a three-step tour explains the essentials. **Esc** or sending your first
request dismisses it permanently. Its marker is kept in Dial's private user state, not the project.

## Steer

While Dial works, type a correction or another request. Follow the input's busy choices to
steer the current work or queue the request. Be explicit about scope: “Stop after the tests;
don't commit.” Use **Esc** to interrupt when you need to stop rather than adjust the work.

## Review

After edits, the compact turn report summarizes changed files, checks, unfinished tasks,
and whether checkpoint undo is available. **Ctrl+O** expands the report along with tool details.
The muted next-step line suggests useful actions; plain answers do not add one.

Use `/review` for a review of your changes. Read failures and stale-check notices before treating
work as done. A current check is not necessarily a passing check; inspect the expanded report
for the recorded results. Dial does not claim shell or extension effects are checkpoint-reversible.

## Undo

Use `/undo` to revert checkpointed edits. Read the report's reversibility warning: shell commands
and extensions can have effects outside checkpoints. Undo is not a general rollback of every
external action.

## Resume

Use `/resume` to find a previous conversation, or pick one from the home screen's recent list.
After an interruption or failure, send a new message to continue with the saved session and
its unfinished tasks. Tell Dial what to retry or change rather than starting over unnecessarily.

## Settings and models

Use `/settings` for preferences and **Ctrl+P** to discover actions.
Use `/models` for model selection, roles, downloads, favorites, and filters—all in one panel.
Keep permission and sandbox choices appropriate to the project, and read approval prompts
before allowing risky actions.
