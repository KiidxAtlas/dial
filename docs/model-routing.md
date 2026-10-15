# Model routing

`[model_roles]` assigns provider/model selections to the roles in the shared role registry, including `task` for implementation tasks and worker subagents. Legacy keys migrate as follows: `smol`, `tiny`, `commit`, `compaction`, and `architecture` become `small`; `subagent` becomes `task`; `slow`, `stronger`, `advisor`, `feedback`, `feedback-helper`, and `experience` become `coach`; `plan` becomes `planner`. Current keys such as `task`, `small`, and `memory` stay unchanged. A legacy `default` key fills unassigned helper roles, including `task`, and `vision` is dropped. Explicit current role selections take precedence over legacy aliases, so assigned `task` and `small` models are preserved independently.

In the TUI, `/roles` opens role assignments. Select a role with Enter, then search or
browse providers and favorites to choose its model. Switching provider groups keeps
the chosen role; Enter assigns to that role, and Escape returns to its row without
changing it. Background catalog refreshes preserve the selected role. Selecting the
Roles group explicitly leaves the current assignment flow.

Use Ctrl+S in the picker to choose Global or Project scope. An explicit model assignment
in either scope takes effect in the current project. A Global assignment saves the new
default and clears only this project's override for the selected role (including an
explicit “follow main” marker); other roles, settings, and other projects' overrides
remain unchanged. A Project assignment changes only this project. Choosing the main
model globally also clears this project's model, provider, endpoint, profile selection,
and context-window override so the old route cannot mask the new choice.

`[retry]` bounds provider retries. `max_attempts` defaults to 10, `max_delay_secs` to 300, and quota resets are not waited out unless `wait_for_quota_reset = true`. `[retry.fallback_chains]` is read only for the main session, keyed by `default` or by an exact `provider/model` identity. Each value is an ordered list of bare model names (same provider), qualified `provider/model` selectors, or `@role` references. Other keys are accepted but never consulted. Recoverable throttling, quota, transient, and authentication failures may proceed to the next model; payload and fatal errors do not.

Example:

```toml
[retry.fallback_chains]
default = ["openai/gpt-4o-mini", "@small"]
"anthropic/claude-sonnet-4-5" = ["openrouter/anthropic/claude-sonnet-4-5"]

[model_roles]
small = { provider = "openai", model = "gpt-4o-mini" }
task = { provider = "anthropic", model = "claude-sonnet-4-5" }
```

Model capabilities and supported reasoning levels come from the bundled catalog when known, with explicit operator profiles remaining authoritative. Catalog context windows are used when available; otherwise remote requests use a conservative 32768-token capacity. Output budgets remain separately bounded rather than reserving the entire context window.

## Model roles in the picker

**Model roles.** The model picker (/roles) assigns the roles in the shared registry. `/improve models` additionally exposes the learning decision role. Each row shows the effective model and where it comes from. An unassigned chat role uses the main model, except `judge` (the `small` role, then the main model). `decision` uses the installed Laya checkpoint, if any; unassigned `embedding` uses lexical recall. “Use the main model” clears a chat-role assignment. The `memory` role is required while a memory backend that calls a model (`local` or `sharpshooter`) is enabled; /roles marks it required until assigned.
Missing memory model assignments do not prevent Dial from opening: the selected memory backend stays inactive with a setup error. Assign its required roles in /roles, then restart Dial to activate memory. No backend or chat-model fallback is silently selected.

In the picker, `Tab` switches between the sidebar and the list; `Enter` or `Right` moves from the sidebar into its selected list. In **Roles**, highlight a role and press `Enter` to choose its model, or `x` / `Delete` to clear an explicit assignment (the main model cannot be cleared). Role selection starts with a fresh search and kind filter; `Esc` returns to the same role. `Ctrl+A` on a model opens the role-assignment chooser. For the Laya checkpoint entry, pasted text goes into the checkpoint field.

| Role | Used for | Falls back to |
|---|---|---|
| `main` | Conversation and coding agent | Always assigned (the session model) |
| `task` | Implementation tasks and worker subagents | For task-tool delegation: `coach`, then `small`, then `main` |
| `small` | Compaction, commit messages (all tiers), architecture synthesis, solve patch-search workers; preferred when assigned for scout and custom read-only subagents | `main` for maintenance; task-tool subagents follow `task` when `small` is unassigned |
| `judge` | Cheap typed decisions, such as `thinking = "auto"` (one reasoning level per user turn, judged from its prompt) and cached locally. Can be a chat model or the Laya decision engine (“laya/decision checkpoint”) | `small`, then `main` |
| `coach` | Escalate tool and automatic escalation, self-improvement feedback proposals, experience teacher, advisor mode, feedback-helper edit repair | `main` |
| `tester` | Reviewer subagent (including `/security` scan batches and finding validation), test-runner subagent and review-workflow model calls | Task-tool reviewer/test-runner: `coach`, `task`, `small`, then `main`; other review workflows: `main` |
| `planner` | Plan mode and planner subagents | Task-tool planner: `coach`, `task`, `small`, then `main`; plan mode: `main` |
| `memory` | Memory extraction and consolidation (`local`) and sharpshooter model calls, including sharpshooter decision memory (`memory.sharpshooter.model_role`). Required when a model-calling memory backend is enabled | No chat fallback for model-calling memory; backend stays inactive until assigned |
| `decision` | Laya decision engine: the assigned value is the Laya checkpoint (local path or HF repo id), passed to the managed service as `DIAL_LAYA_CHECKPOINT`. Set with `dial laya install --checkpoint` or /roles | Installed Laya checkpoint (shown as “Laya · <checkpoint>”), else not set |
| `embedding` | Semantic memory recall through an embedding-capable endpoint | Lexical recall |

### Task-tool routing and deliberate overrides

Delegated jobs choose the first **explicitly assigned** role in the following purpose-specific chains:

| Task type | Default model-role chain |
|---|---|
| Implementation worker | `task` → `coach` → `small` → `main` |
| Scout / custom read-only | `small` → `task` → `main` |
| Reviewer / test-runner | `tester` → `coach` → `task` → `small` → `main` |
| Planner | `planner` → `coach` → `task` → `small` → `main` |

Main is the last resort when no helper in that chain is assigned. Dial does not select an unconfigured catalogue model or provider. These are missing-assignment defaults, not provider-error retry chains: they do not silently restart a job on another model after work has begun. They apply only to task-tool delegation; unassigned maintenance roles retain their existing behavior.

Explicit operator agent definitions and an explicit per-call `model_profile` are deliberate overrides of this routing. They are not silently replaced by the preferred helper role or by `task`.

## Judge role and automatic thinking

The `judge` role makes cheap typed decisions: it picks one answer from a fixed list and returns it as `{"choice": "..."}`. Its model is `[model_roles].judge` when assigned; otherwise it follows `small`, then the main model. Answers are cached in SQLite under Dial's user directory (`cache/judgment.db`; override with `DIAL_JUDGE_CACHE_DB`) for 30 days, keeping the 5000 most recently used rows. The cache key covers the model identity, question, choices and context, so a changed model or prompt is asked again.

The judge can also be Laya. When choosing the judge model in /roles, the first entry is **laya/decision checkpoint**. It sends each judgment to Dial's managed Laya service as one typed `choice` question and needs no chat model. It uses the `decision` role's checkpoint, or the installed one, so the judge and decision advice share one service. The entry cannot be selected until a checkpoint exists (`dial laya install --checkpoint PATH` or the `decision` role). In TOML this is `[model_roles.judge] provider = "laya"`, `model = "decision"`; any other Laya model id is rejected. Laya judgments do not need `/laya on`, which controls retrieval advice only. A cold Laya start can delay the first judged prompt by up to the Laya request timeout (30 seconds when `timeout_secs` is 0). Cached answers are keyed by the checkpoint path.

`thinking = "auto"` (`--thinking auto`, `/effort auto`, or the Auto thinking setting under Requests) judges each new user prompt once, before its first request, and chooses off, low, medium or high. The choice is clamped to the model's declared reasoning efforts. The judgment is not repeated for each tool step. A fixed level (`--thinking <level>` or `/effort <level>`) clears auto. If no judge model can answer, the call fails, or the reply is not a listed choice after one retry, Dial keeps the model's default effort and shows a diagnostic. Judging never runs in hermetic agents.
