# Design philosophy

What Dial is for, the principles behind its behavior, and how to decide when they conflict.
Use this page to settle design arguments. [`ARCHITECTURE.md`](../ARCHITECTURE.md) states the
enforced invariants that these principles produce.

[Documentation index](README.md)

## What Dial aims to be

**Dial is the coding agent you can hand real work to, because you can always see what it did,
check that it worked, and take it back.**

Other agents compete on how much they can do unattended. Dial competes on how safely and
verifiably it does things: the delegation is _trusted_, not just _capable_. The model may be a
frontier API or a 3B model on a laptop. Whichever it is, Dial's job is to keep the work bounded,
reviewable and reversible, and to make the model's output better than the model alone would make it.

In one line: **a trustworthy harness, model-agnostic, terminal-first, owned by the user.**

### What it is not

- **Not a chat app with tools bolted on.** A conversation that ends with "done!" but no evidence
  is a failure, however fluent it sounds.
- **Not an autopilot that trades safety for speed.** Full autonomy is available, but it is always
  something the user chooses explicitly, never a default or an accident.
- **Not tied to one vendor.** No feature may require a specific hosted model when a local or
  alternative route can reasonably provide it.
- **Not a platform for every idea.** A capability earns a place by serving the core promise
  below. Experiments live behind `experimental.*` settings until they show measured value.

## The core promise

Every user-facing decision should protect these four guarantees. They are ordered: when two
conflict, the earlier one wins.

1. **Nothing is lost.** The user's files, uncommitted work, drafts and sessions survive crashes,
   interruptions, cancellations and the agent's mistakes. Every edit is checkpointed and undoable
   (`/undo`, `/rollback`, `/review-file`), and destructive git operations are guarded.
2. **Nothing happens without authority.** Actions run inside the permission the user granted:
   approvals, sandbox, workspace confinement. Missing protection fails closed. Loading a tool,
   a peer message, a file's contents or a model's suggestion never grants permission.
3. **Nothing is claimed without evidence.** "Done" means the relevant checks were run and their
   real results are shown. Skipped checks are named as skipped, and failures the turn did not cause
   are disclosed rather than hidden.
4. **The user stays in control of the turn.** They can see what is running, steer it, queue for
   later or stop it at any moment, and know in advance what an action will do to the work in flight.

Speed, cleverness and breadth come after these four, never before.

## Principles

Each principle includes the reason for it and a concrete test for whether a change follows it.

### 1. Reversible by default, explicit when not

Any operation a user cannot cheaply undo needs an explicit, informed choice at the moment it
happens. Reversible operations should be fast and free of friction.

- _Why:_ trust grows when mistakes are cheap. A user who knows every edit can be undone delegates
  more, and more usefully.
- _Test:_ can one stray keypress, a mistyped command or a model error cause loss or a large
  increase in authority? If so, the change is wrong. (Shift+Tab never cycles into full auto for
  this reason.)

### 2. Evidence over assertion

The harness, not the model's phrasing, decides when work is finished. Claims cite what was
observed: commands run, exit codes, paths and lines.

- _Why:_ models produce confident text whether or not the work succeeded. Dial's value is
  everything a model cannot vouch for itself.
- _Test:_ if the model stated success while every check failed, would Dial accept it? It must not.

### 3. Fail closed, explain why

When a protection, dependency or capability is unavailable, Dial refuses and says what is
missing and how to fix it. It never quietly falls back to a weaker mode.

- _Why:_ a silent downgrade breaks every guarantee above without anyone noticing.
- _Test:_ does every refusal name its cause and a next step (`/doctor`, `/setup`, a setting)?

### 4. The harness makes the model better

Dial should lift a weak model rather than merely tolerate one: a stable, budgeted tool surface;
schema validation and repair before execution; focused verification; memory and project
understanding; evaluation-gated learning from the user's own sessions.

- _Why:_ "any model, including local" only matters if local models can do real work.
- _Test:_ is the improvement measured on a benchmark or held-out set before it becomes a default?
  Unmeasured cleverness stays experimental.

### 5. The user's machine, the user's data

Secrets and private state live outside the workspace. Local models are first-class. Network use
and paid actions are visible and explicitly approved. Learned adapters stay on the user's machine
and are adopted only after passing an evaluation gate.

- _Why:_ a tool with this much access has to be one the user can fully own.
- _Test:_ could this change leak a secret into a log, transcript, commit or provider request, or
  spend money without explicit consent?

### 6. Calm, legible interface

The terminal shows what matters now and keeps everything else one step away. State is visible
(model, approval mode, what is running, what is queued), and actions say what they will do before
they do it.

- _Why:_ an agent that runs for minutes needs a surface people can glance at, not study.
- _Test:_ can a new user tell, without docs, what the agent is doing, what pressing Enter will
  do, and how to stop it?

### 7. One way to do each thing, with discoverable shortcuts

Each concept has one canonical command and one obvious key. Aliases exist for compatibility and
habit, not as competing routes, and they stay hidden from discovery. Descriptions say what the
command does in plain words, not internal codenames.

- _Why:_ 141 commands and overlapping keys are the largest source of UX friction Dial has.
- _Test:_ does a new command duplicate an existing hub or tab? Could an existing command's
  subcommand or a setting cover it instead? Would someone guess what it does from its name and
  one-line description?

### 8. Predictable over magical

The same input does the same thing in the same state, across platforms where the terminal allows.
When behavior must depend on state (Ctrl+C clears a draft before it stops a turn), the current
meaning is shown in a hint before it happens.

- _Why:_ surprises with authority attached destroy trust faster than missing features.
- _Test:_ is the context-dependent meaning visible in the interface at the moment it applies?

### 9. One owner per concern

Every behavior has one implementation that every surface (terminal, CLI, RPC, workers) consumes:
one command table, one settings schema, one completion decision, one tool contract. New features
extend the owner rather than adding a parallel path.

- _Why:_ duplicated behavior drifts, and drifted behavior breaks principle 8.
- _Test:_ do the UI, the CLI and validation all read this from the same declaration?

### 10. Compatibility is a promise

Recorded sessions, tool contracts, settings files and keybindings keep working across releases.
Changing a contract requires a migration or a legacy projection, not a break.

- _Why:_ a session worth resuming is a session worth protecting.
- _Test:_ would a session recorded yesterday resume, replay and undo correctly after this change?

## Resolving conflicts

When principles pull in different directions, decide in this order:

1. Safety of the user's data and authority (promise 1–2, principles 1, 3, 5).
2. Honest reporting (promise 3, principle 2).
3. User control and predictability (promise 4, principles 6, 8).
4. Model capability and output quality (principle 4).
5. Speed and token cost.
6. Breadth of features.

For example, parallel background checks are opt-in (`experimental.background_check_slots`)
because faster verification (5) must not risk interleaved workspace state (1). Busy-time messages
open a chooser by default because user control (3) outranks saving a keypress (5).

## Checklist for a new feature

Before adding a capability, command, key or setting, answer:

- Which part of the core promise does it serve, and which could it weaken?
- Can it extend an existing command, hub, tab or setting instead of adding a new one?
- Is it reversible? If not, where does the explicit, informed choice happen?
- What is the failure mode, and does it fail closed with a clear explanation?
- How is the benefit measured, and should it start as `experimental.*`?
- Does it work with local models and on every supported platform, or does it say why not?
- Does it keep recorded sessions and contracts compatible?
- Is the user-visible behavior documented, with a CHANGELOG entry?

## Where Dial should go next

These directions follow from the philosophy and from known friction:

- **Fewer, clearer entry points.** Consolidate overlapping commands into hubs and tabs, keep old
  spellings as hidden aliases, and retire internal codenames from discovery.
- **Show consequences before they happen.** Extend the "stops the current turn" labeling to every
  action whose effect depends on state: approvals, model changes and session-replacing commands.
- **Layout that explains itself.** When width or height hides a panel, say so and offer the key
  that shows it, instead of letting content disappear silently.
- **Measured defaults.** Promote experimental features only when a benchmark or held-out
  evaluation shows benefit, and record the evidence with the change.
- **Local models as peers.** Keep closing the gap with tool grammar, a lean tool surface,
  verification-driven repair and gated adaptation, so the trustworthy harness is useful offline.
