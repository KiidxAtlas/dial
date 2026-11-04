# Sessions and history

Resume, search, branch, share and move conversations while preserving their recorded history.

[Terminal overview](terminal-interface.md) · [Documentation index](README.md)

For independent Dial sessions in the same checkout, `/peers` lists live peers and your own ID;
`/peers send <id> <text>`, `/peers claim <paths...>` and `/peers clear` exchange messages and
manage advisory path claims through ordinary transcript notices. Commands wait until an active
turn finishes. See [Session coordination](session-coordination.md) for delivery, safety and opt-out details.

- `/sessions` and `/resume` open the current project's conversation picker. Pinned sessions lead,
  then date groups (Pinned, Today, Yesterday, This week, Older; headers are not selectable), each
  ordered by journal activity rather than creation time. Empty sessions are hidden unless pinned;
  **Tab** includes them, and the hidden count appears only when some are hidden. Titles take priority
  over IDs and model details; the selected conversation shows its latest prompt and answer, and in
  wide terminals (100+ columns, 18+ rows) a preview pane shows its first and last turns. Search
  matches words in any order across titles, prompt/answer previews, model and ID, with exact title
  matches first. The header reads `showing N of M` when the first page (200 per kind) does not hold
  every journal; **Ctrl-A** loads all of them, and a search that matches nothing in the loaded set
  says so.
  Keys: **Ctrl-U** clears search; **Esc** clears a non-empty search first and closes on the next
  press; **Ctrl-P** pins/unpins; **Ctrl-R** renames the selected session (edit the title in the
  search line, **Enter** saves); **Ctrl-F** forks it; **Ctrl-D** deletes it after confirmation (the
  current session is deleted with `/delete`); **Ctrl-G** toggles this project and all projects (all
  projects shows each session's project directory; resuming a session from another project is
  refused with a pointer to `/move`); **Home/End** and **PageUp/PageDown** navigate; **Enter**
  resumes. A session another Dial process has open is marked `[open elsewhere]`; choosing it creates
  an independent sibling fork and identifies the source, without taking over its writer or changing its bytes.
  `/resume <id-prefix>` opens a unique ID directly, including empty sessions; an ambiguous prefix
  opens the picker with that prefix as its search. Choosing the current session notes
  `Already in session`. After a resume, the note shows the title, model, last activity and the
  recorded state. `/tree forks` shows fork depth across sessions: each fork is indented under its
  parent, and siblings are ordered most recent first; it includes empty sessions initially. The picker
  keeps up to 200 conversations and 200 empty sessions; listing occurs once per open, and each action
  reloads the list rather than running per keystroke.
  `dial --restore` opens the parent conversation first, then explicitly rebinds eligible interrupted
  workers through the normal safe resume path. Use `-r <id> --restore` to name the parent; without
  an ID it continues the latest conversation. It does not resubmit the main prompt, replay completed
  calls or restart completed/cancelled workers. Refused workers remain interrupted with a reason.
- `/search <query>` searches past messages using the existing local full-text index. Index
  maintenance and SQLite waits run off the interactive command path, both while idle and during
  a turn. Repeated searches retain only the latest pending query; superseded results and results
  from a previous session or workspace are discarded. This does not change the search ranking or
  send the query to a model. Search verifies the indexed byte prefix on every query, so even
  same-size, same-inode rewrites with restored timestamps invalidate old message text. Appends
  are parsed incrementally, but verification still reads prior indexed bytes; its cost grows with
  history size. No search-latency improvement is asserted.
- Session moves keep the source journal until the destination and workspace switch are accepted.
  Failed moves settle the copied journals and transfer-ledger state together; an uncertain
  publication or cleanup is reported rather than described as a successful rollback. Workspace-root
  header replacements flush the replacement and, on Unix, its parent directory. A failure after
  replacement is reported as applied with uncertain durability, not as an unchanged session.
- `/tree` opens the session's own entry tree: every user message and labeled entry, including
  branches abandoned by a rewind, indented where the conversation forked (filled markers are on the
  branch the model sees, hollow ones are abandoned; `<- here` marks where you are). Type to filter
  by text or label, **Enter** moves there. The preview uses the conversation's native transcript
  renderer, including tool evidence. **Ctrl-Up/Down** jump between user turns, **Ctrl-Left** goes
  to the visible parent, and **Left/Right** select siblings. Choosing a user message rewinds to just
  before it and loads its text only when the composer is empty; an existing draft is preserved.
  Choosing a labeled answer continues from it. The move appends a branch marker, so nothing is
  deleted and the previous path stays in the tree. This is conversation navigation, not file rollback;
  use the checkpoint/review controls to change workspace files.
  `/label <text>` bookmarks the latest message (up to 60 characters; `/label` alone clears it;
  the newest label on an entry wins). Labels are journal records the model never sees. Session
  journals carry an integer `schema_version` in their header; older journals load unchanged and a
  journal from a newer Dial is refused with a message to update.
- `/btw <question>` starts a streamed, tools-free side conversation without stopping the main turn.
  Busy questions use the frozen turn-start context, including its active user prompt; they do not
  read the running agent's mutable state. Follow-up questions continue the open thread, and
  `/btw new <question>` starts another. `/btw threads` lists them, `/btw open <id>` displays one,
  `/btw copy` copies its last completed answer, and `/btw cancel` stops only the side request.
  Stopping the main turn does not cancel a side answer. Saved side history is private and bound to
  its parent session; it is not injected into the main model conversation.
  Admitted questions and streamed prefixes are checkpointed before display. Reopening after a
  crash retains the observed prefix as an interrupted exchange and does not repeat the request.
  The private sidecar follows session move/archive/delete operations; independent forks do not
  take over a live side request.
  `/btw promote <id>` creates a full sibling conversation from the saved ancestry and completed
  exchanges. It leaves the main branch, draft and jobs unchanged and returns a `/resume` target.
  Promotion waits for an idle main owner; ordinary side commands run while it is busy.
- `/branch` with no argument opens a list of the nine most recent user messages (keys 1-9);
  `/branch <n>` names one directly. Either way the rewind asks first and says how many messages
  leave the view. `/delete` and `/session delete` ask before removing the current session file;
  cancelling keeps it.
- `/rename <title>` names a session; `/export [path] [--html]` exports Markdown or self-contained
  HTML without overwriting existing files. `/copy` copies the last answer; `Ctrl-G` opens the
  draft in an external editor.
- `/pack [path]` writes the same archive as `dial session pack`: one zip with the journal, side
  history, stream journal, subagent transcripts and referenced artifacts, each recorded in a manifest with
  a SHA-256 and byte count. `dial session unpack <file>` (or `dial sessions unpack`) installs it into the
  current project. Unpacking verifies every member and artifact before anything is written, refuses path
  traversal, absolute or backslash names, symlinks, oversized members, corrupt bytes, and archives whose
  format or journal schema is newer than this Dial, and never overwrites an existing file. The transfer is
  recorded, so reopening the original path after the session moved away forks a sibling instead of
  reattaching to it.
- `/export` and `dial sessions export` (Markdown or HTML) and `/dump all` include the subagent transcripts
  that belong to the session, after the parent conversation and with the same redaction. `/dump anon`
  anonymizes them the same way it anonymizes the session.
- `/share` and `dial sessions share` seal the transcript under an 8 MiB cap. When it does not fit, they
  trim in order: inline images are replaced with an `unavailable` marker, long strings are truncated at
  32768, 8192, 2048, then 512 characters, and only then the oldest records are dropped (the session header
  is always kept). Each trim applied is reported. A configured `[share].server_url` (or
  `DIAL_SHARE_SERVER`) receives the share; without one, or with `--gist`, it becomes a secret GitHub gist
  created through `gh`. If `gh` is missing or unauthenticated the command fails with an actionable error
  and does not fall back to another host.
