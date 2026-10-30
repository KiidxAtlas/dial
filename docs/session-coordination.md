# Session coordination

Two terminal tabs can run independent Dial sessions against the same checkout. Project peer
coordination gives them lightweight presence, advisory path claims and bounded messages, without
model calls to discover or contact peers. It does not combine conversations, automatically isolate
work or infer semantic dependencies between tasks.

## Terminal commands

```text
/peers
/peers list
/peers claim crates/example/src 'docs/a page.md'
/peers send <id> I am changing the parser; please leave its public interface stable.
/peers clear
```

- Bare `/peers` opens the peers panel (see below). `/peers list` prints the same roster to the
  transcript as text, with every live peer's full ID and this session's own. Independent sessions
  are not delegated workers: use `/peers`, not the worker messaging controls, to reach another tab.
- `claim` replaces this session's advisory relative-path scopes. Quote paths containing spaces.
  Claims communicate intended ownership; they are not locks, edit permissions or exclusive access.
  Claims last through the next turn and clear when that turn becomes idle; use `clear` to release
  them earlier. Normal same-session settings/model rebuilds retain the peer ID and queued mail.
- `send` queues guidance for a live peer. Messages and inboxes are bounded; check the returned result
  rather than assuming delivery. During a turn, guidance reaches the next model-request boundary;
  it does not interrupt an in-flight model response or tool call. In an open terminal, fresh mail
  can also wake an eligible idle conversation (see below). Queuing is not acknowledgement.
- `clear` clears this session's claimed scopes, not its conversation or another peer's state.

The subcommands write ordinary transcript notices. During a running turn, `/peers` and its
subcommands wait for the turn to finish. The model-facing `peers` tool exposes the same operations:
`action="list"` (the default), `claim` with `scopes` and optional `summary`, `send` with `to` and
`text`, and `clear`.

Automatic model-facing roster notes are a bounded coordination preview, not a repeated full peer
inventory: at most eight identities/lifecycle states, overlapping peers first, with at most four
overlapping scopes per previewed peer and explicit omitted counts. Unrelated peers' changing task
summaries do not regenerate that note. Explicit `peers list`, slash-command rosters and the panel
retain the full view; fresh peer messages are delivered independently and are never hidden by roster
deduplication.

### The peers panel

Bare `/peers` opens a board of the sessions in this checkout:

- **This session**: its short peer ID, phase (`working` or `idle`), task summary and claims, plus
  the inbox: how many peer messages are queued for this session that its model has not received
  yet. Opening the panel only counts them; they are still delivered at the next model request.
- **Other sessions**: every live peer with its short ID, phase, task summary and claims.
- Claims that overlap a claim on the other side (one path contains the other, compared by path
  component) are marked with a warning sign and the warning color. The title shows the peer
  count, how many peers overlap this session's claims, and the unread count.

Keys: `↑`/`↓` (or `Home`/`End`) select a peer; `Enter` or `m` writes a one-line message to the
selected peer (`Enter` sends, `Esc` cancels the message); `r` refreshes; `c` clears this
session's own claims; `Esc` closes. The panel is a snapshot taken when it opens and after each
action: it does not poll, so press `r` to see newer peer state. Each send or clear reports its
outcome in the panel. The panel never changes another session's claims.

## Task-scoped guidance and idle wake-ups

Peer guidance can influence the next step **within the existing user-authorized task**. It cannot
replace the user's request, expand permissions, answer a human approval, or supply a new task.
The message stays labelled untrusted peer context rather than becoming a typed prompt, slash
command, attachment reference, or turn-drafter input.

With `coordination.auto_wake = true` (the default), an open terminal session checks for mail once
per second while its last turn is **Completed** or **Incomplete**. At most two fresh messages are
admitted for each wake-up, with at most **three automatic wake-ups per user turn**, separated by
at least **30 seconds**.
Repeated sender/text pairs in the most recent 32 delivered messages do not trigger another wake-up.
These continuations can incur model charges; normal tool approvals, sandbox rules and compute
admission still apply. No extra process or headless session is launched.
Relevance is assessed by the model after waking, not by a pre-inference filter: even unrelated
fresh mail can consume a request. Disable automatic wake-ups below to avoid that cost.

Blocked, failed, interrupted, and human-input-waiting tasks do not wake automatically. A completed
reply can wake for relevant peer follow-up within the existing task or conversation; completion
does not authorize new work or permission changes. Without fresh mail, no inference starts.
No task is armed on process restart or session resume; a real user turn must first end successfully.
Normal same-session settings/model rebuilds retain the bounded wake budget. A different task or
session cannot inherit an armed wake-up. Queued human input, a nonempty composer, open panels,
and scheduled loop turns take priority. Esc at the idle composer suspends wake-ups until the next
user turn. Cancellation or failure during wake admission also suspends them, retaining reserved
mail for a later normal request.

To keep guidance delivery but disable automatic inference, set:

```toml
[coordination]
auto_wake = false
```

This setting is also under Tools → Session coordination → Peer wake-ups in `/settings` and takes
effect in a new session. RPC, ACP and one-shot CLI clients still receive mail only when they run a
foreground request; they do not acquire a new idle event loop.

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
that unrelated or later changes were tested.

## Shared build and test results

Sessions in one checkout share the results of check commands (`cargo test`, `cargo check`,
`npm run build`, `pytest` and similar), so two tabs do not compile the same tree twice. Before such
a `bash` command runs, Dial fingerprints the tree from git: HEAD, the content of every uncommitted
or untracked file (ignored build output excluded), the exact command text and its `cwd`. Then:

- If another live session already **finished** that command on an identical tree, the call returns
  that result (status, exit code, the output tail and the peer's complete-log artifact link),
  labelled as not run here.
- If another session is **still running** it, the call is not started; this session joins the run
  and gets its result as a notice once it settles, so it can keep working meanwhile.
- Otherwise the command runs normally and its run is announced. A run that moves to `bg` is
  published when the job exits.

Each shared answer is given once: running the same command again executes it in this session.
A shared result is a peer's observation. It never counts as this session's own verification or
check evidence. A result is published only when the tree fingerprint is the same at the end of the
run as at the start; cancelled or timed-out runs, and runs whose tree changed meanwhile, withdraw
their announcement. Runs of a session that exits are dropped. Outside a git repository with at
least one commit, or when a changed file cannot be hashed, nothing is shared. The ledger lives
next to the peer registry in private storage, and is bounded to 64 entries with a 2 KiB output
tail each. Disabling coordination disables sharing too.

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
