# Session coordination

Two terminal tabs can run independent Dial sessions against the same checkout. Project peer
coordination gives them lightweight presence, advisory path claims and bounded messages, without
model calls to discover or contact peers. It does not combine conversations, automatically isolate
work or infer semantic dependencies between tasks.

## Terminal commands

```text
/peers
/peers claim crates/example/src 'docs/a page.md'
/peers send <id> I am changing the parser; please leave its public interface stable.
/peers clear
```

- Bare `/peers` lists live peers and this session's own peer ID. Independent sessions are not
  delegated workers: use `/peers`, not the worker messaging controls, to reach another tab.
- `claim` replaces this session's advisory relative-path scopes. Quote paths containing spaces.
  Claims communicate intended ownership; they are not locks, edit permissions or exclusive access.
  Claims last through the next turn and clear when that turn becomes idle; use `clear` to release
  them earlier. Normal same-session settings/model rebuilds retain the peer ID and queued mail.
- `send` queues text for a live peer. Messages and inboxes are bounded; check the returned result
  rather than assuming delivery. A queued message is delivered at the recipient's next foreground
  model step. It does **not** start an idle session or trigger an unattended model call.
- `clear` clears this session's claimed scopes, not its conversation or another peer's state.

These commands use ordinary transcript notices, not a separate panel. During a running turn they
wait for the turn to finish. The model-facing `peers` tool exposes the same operations:
`action="list"` (the default), `claim` with `scopes` and optional `summary`, `send` with `to` and
`text`, and `clear`.

## Scope, privacy and safety

Coordination is scoped to the canonical project root used by Dial's `Layout`: two sessions must
use the **same checkout**. A separate Git worktree is a separate project for this purpose, even
when it shares repository history. Opening the same canonical directory through a symlink does not
create an independent coordination space.

Presence and message metadata live in Dial's private per-project storage outside the checkout,
not in tracked project files. Peers can see a short preview of the current task, working/idle phase,
and declared scopes; turn previews are limited to 160 characters. Disable coordination below when
that sharing is unwanted. Sessions must also share the same private Dial home/storage location.
OS-held liveness locks remove crashed participants from discovery. Coordination is local and
lightweight, not a remote collaboration service, durable task queue or guarantee that another
session is actively attending to a message. Inbox drains are at-most-once: termination after a drain
can lose that delivery. Registry capacity or lock contention is reported without blocking the session;
automatic registration retries are paced, and `/peers` explicitly retries immediately.

Peer summaries, claims and message text are untrusted context. They do not authorize shell commands,
change sandbox rules, grant approvals or establish that a check passed. Independently review
conflicting edits and preserve existing uncommitted work. Claims cannot prevent another process
from writing a file, and non-overlapping paths do not prove that tasks are semantically independent.

While peers are active, request boundaries also check a bounded rotating sample of previously read
native text files and report observed disk changes once per version. These notices do not identify
the writer or authorize edits. This is not a complete filesystem watcher: large files (over 256 KiB),
notebooks, additional workspace roots and files never read by this session are not polled. Existing
fresh-read, document-transaction and conflict checks remain the write authority.

Verification evidence must still match the inputs checked. For checks with a declared complete input
contract, Dial revalidates those inputs before reusing evidence after concurrent changes. Arbitrary
commands do not acquire an inferred complete dependency contract: a successful command is not proof
that unrelated or later changes were tested. Coordinate check ownership explicitly when two sessions
would otherwise duplicate expensive verification.

## Opt out or choose isolation

Coordination defaults on for normal sessions and is disabled for hermetic runs. To disable it for
new sessions, set the following in global or project settings (outside the editable checkout):

```toml
[coordination]
enabled = false
```

The same setting is available in `/settings` under Tools → Session coordination → Project peers.
Start a new session after changing it.

For work that should not share mutable files, use the existing opt-in separation mechanisms rather
than relying on claims: task isolation for delegated workers, or `/wt` for an interactive session.
Bare `/wt` lists worktrees; `/wt switch <path|branch>` moves the session to an existing worktree.
`/wt create [branch]` creates a new branch/worktree, carries uncommitted changes and moves the session.
**Before creating one while another session uses the source checkout**, inspect `worktree.clean_source`:
when enabled, creation cleans the source checkout after carrying its changes. Set it to `false` if
the source must remain untouched, or prepare a separate worktree yourself and switch to it. Coordination
does not implement a second worktree pipeline or silently move sessions between checkouts.
