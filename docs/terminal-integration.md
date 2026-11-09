# Terminal integration

Use terminal links, titles, progress signals and the optional live side pane.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

## Terminal integration

With `ui.terminal_integration` on (the default; `/settings` → Interaction → Notifications), Dial
tells terminals that support it more than the cells it draws. Each item is sent only to terminals
known to implement it, detected from `TERM_PROGRAM`, `LC_TERMINAL` and similar variables:

| Signal | Terminals | What you see |
| --- | --- | --- |
| Links (OSC 8) | iTerm2, Ghostty, kitty, WezTerm, VS Code, foot, Rio | URLs and file paths that exist, resolved against the project, are clickable in scrollback. In iTerm2, `path:line[:col]` opens at that line through Semantic History (⌘-click, using the editor set in Profiles → Advanced). The live region and text that is not a real URL or path are never linked. |
| Working directory (OSC 7) | iTerm2, Ghostty, kitty, WezTerm, foot, tmux | New tabs and splits start in the project. |
| Tab circle | any terminal that shows titles | A colored circle ends the tab title so it stays visible when iTerm2 shortens an unselected tab from the left: orange while working, red when an approval or question waits or a turn failed, green when a turn finished. A key press clears it. The title is `dial · <project> · <session title> <circle>`, where the session title is its name (`/rename`) or its first message. Off with `ui.terminal_integration = false`; omitted with ASCII glyphs. |
| Attention | iTerm2 | The dock icon bounces while an approval or question waits. |
| Progress (OSC 9;4) | iTerm2 3.6+, Ghostty 1.2+ | Prompt processing percentage, an indeterminate bar while generating, paused while waiting for you. |
| User variables | iTerm2, WezTerm | `user.dialState` (`idle`, `working`, `waiting`, `done`, `failed`), `user.dialModel` and `user.dialContext` (context used, such as `42%`) for status bar components, badges (`\(user.dialState)`) and tab titles. |

Under tmux, terminal-only sequences go through passthrough, which needs `set -g allow-passthrough on`.
Links are off there because tmux does not keep them by default. Zellij and GNU screen get none of
these. Dial restores progress and variables when it exits, suspends or crashes. The older
`ui.hyperlinks` key is still accepted but ignored.

### Live side pane

`/watch` enables a private live feed for the current session and runs immediately, even during a
turn. It then tries to open the follower in a split to the right of Dial, keeping focus on Dial.
The innermost layer wins, because a multiplexer inherits the outer terminal's variables:

| Detected by | Split command |
| --- | --- |
| `$TMUX` (targets `$TMUX_PANE`) | `tmux split-window -h -d -t PANE -- dial …` (tmux 3.0 or newer) |
| `$ZELLIJ` or `$ZELLIJ_SESSION_NAME` | `zellij run --direction right -- dial …` |
| macOS, `TERM_PROGRAM=iTerm.app` | iTerm2 vertical split of that exact session (Automation permission) |
| `$WEZTERM_PANE` | `wezterm cli split-pane --right --pane-id PANE -- dial …` |
| `$KITTY_WINDOW_ID` and `$KITTY_LISTEN_ON` | `kitty @ --to SOCKET launch --location=vsplit --keep-focus -- dial …` |

The watcher's executable and arguments are passed as separate arguments, never through a shell.
GNU screen is refused because its split commands act on whichever region is focused and cannot be
pinned to Dial's window. kitty needs remote control on a socket (`allow_remote_control
socket-only` and `listen_on unix:/tmp/kitty` in `kitty.conf`); Dial does not send remote-control
escapes through the terminal it is drawing. When no supported terminal is detected, or the split
command fails or exceeds five seconds, Dial prints the exact command, using the running
executable's path, to run in another terminal:

```sh
/path/to/dial -C /path/to/project watch SESSION_ID
```

In a terminal the pane is a tabbed dashboard of everything running for the session. Switch tabs with
`←`/`→`, `Tab`/`Shift-Tab` or `1`–`6`; `↑`/`↓` select, `PgUp`/`PgDn` scroll the details, `Home`/`End`
jump, and `q` or `Ctrl-C` closes the pane. The header shows counts and whether a turn is running;
it warns when the session has not published for five seconds (closed, or busy in a step that does
not repaint).

| Tab | Shows | Action |
| --- | --- | --- |
| **Feed** | Edit previews and changed paths, language-server advisories, turn outcomes, `/review` results and screenshots | — |
| **Plan** | The agent's task list with blocked/dropped reasons, findings and evidence | — |
| **Jobs** | Background (`bg`) processes and slash-command jobs such as `/update`, `/gc` or `/benchmark`, with their recent output | `x` stops the selected job |
| **Workers** | Workers with status, current step, usage and recent activity | `x` stops the selected worker |
| **Threads** | `/btw` side threads, including the answer streaming now | `x` stops the streaming answer |
| **Peers** | Other Dial sessions in this checkout, their claims and overlaps with yours | `Enter` opens the chat with the selected peer; `m` writes a message to it |

The peer chat lists the messages this session sent to and received from that peer since it started,
oldest first (the last 50 are shown). Messages still queued for this session are marked **unread by
the agent** until the next model request delivers them; reading the chat never takes them. History
lives only in the running session's memory and disappears with the peer. In the chat, `↑`/`↓` and
`PgUp`/`PgDn` scroll, `m` writes to the peer and returns to the chat, and `Esc` goes back to the list.

Every stop asks `y`/`n` first. Requests go through the same controls the main window uses; each
outcome is noted in the conversation under **Side pane** and shown in the pane's footer. A peer
message written during a turn is sent when the turn ends. Approvals, questions and anything else
that needs a decision stay in the main window; the pane cannot approve, answer or start work.

While the pane is attached, slash-command job output goes only to its **Jobs** tab (the conversation
keeps the job's one-line status and result), and the pinned worker block above the composer is
hidden because the **Workers** tab shows it in full. Closing the pane restores both. Preview diff
lines are coloured; this is a display of tool output, not a new verification result. LSP advisories
require a running server under the usual LSP settings and remain explicitly **not verification**.
`/review` keeps its existing execution and queuing behaviour; opening a watcher does not start a
review or model request.

When the pane is attached, screenshots are copied only from verified attachments and shown in its
**Feed**, with a one-line marker in the conversation instead of an inline thumbnail. If publishing
fails or no viewer is attached, Dial keeps the inline preview. Closing the watcher restores inline
images on the next capture. `dial watch SESSION_ID --once`, or a watcher whose output is redirected,
prints the recorded feed as text instead of the dashboard; redirected output contains no colour or
graphics escape sequences.

Feeds are opt-in per session and stored outside the checkout under the project's private state
directory, `live/SESSION_ID/`: the feed, the dashboard state (`state.json`, rewritten only while a
pane is attached) and the pane's pending requests (`inbox/`, removed as the session reads them).
Switching or forking sessions does not redirect an existing viewer; run `/watch` in the new session.
Text previews are bounded and the append-only text feed stops at 16 MiB rather than silently
deleting earlier entries; screenshot copies stop at 64 MiB per session. These files may contain
source and screenshots; they are local private state, not published or sent to an additional
service.
