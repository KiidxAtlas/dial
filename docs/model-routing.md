# Model routing

`[model_roles]` assigns provider/model selections to the shared role registry. `task` handles implementation workers; `coach` handles planning and stronger advice; `decision` handles typed judgments via a chat model or Laya. The former `planner` and `plan` assignments migrate to `coach`, and `judge` migrates to `decision`; explicit current assignments win. Grounding and image-embedding models remain available as feature settings under `[vision.grounding]` and `[vision.image_embedding]`, with old role selections migrated intact. Former Laya checkpoint assignments move to `laya.checkpoint`. Other legacy aliases (`smol`, `tiny`, `commit`, `compaction`, `architecture` → `small`; `subagent` → `task`; `slow`, `stronger`, `advisor`, `feedback`, `feedback-helper`, `experience` → `coach`) retain their migrations. Legacy `default` fills unassigned helper roles, and retired `vision` is dropped.

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

`[retry]` bounds provider retries. `max_attempts` defaults to 10, `max_delay_secs` to 300, and quota resets are not waited out unless `wait_for_quota_reset = true`. `[retry.fallback_chains]` is keyed by a role name (`default` for the main session, or a worker role such as `task`, `small`, `tester`, `coach`), by an exact `provider/model` identity, or by a `provider/*` wildcard. When several keys match, precedence is: role key, then exact `provider/model`, then `provider/*`; only the first match is used. Each value is an ordered list of bare model names (same provider), qualified `provider/model` selectors, or `@role` references.

`retry.fallback_on` chooses which failures move a request to its chain: `"any"` (default) is throttling, quota, transient and authentication failures; `"usage-limit"` is throttling and quota exhaustion only; `"except-usage-limit"` is transient and authentication failures only. Payload, context-overflow and fatal errors never fall back.

The main session returns to its primary model once the fallback's cooldown (the server's retry-after, else 5 minutes) elapses, when `retry.fallback_revert` is `cooldown-expiry` (the default). With `never`, it stays on the fallback until you switch models with `/model`. A request already in flight is never switched; the return happens at the next request boundary, and both the fallback's first reply and the return to the primary are announced. Delegated workers consult the chain for their role after their configured routes. A worker moves to a fallback model only when its provider failed before it did any work, and is never restarted partway through a job; it does not return to the primary because each worker is a single job. A worker pinned to a named model profile is not substituted.

### Guided backup models

`/model backups` opens an offline draft editor for this same retry policy. Choose cached chat
models, saved favorites/profiles or role references; type on the model-choice row to filter,
use Left/Right to browse, and Enter to append. On the ordered-backups row, Ctrl+Left/Right
selects an entry, Alt+Left/Right reorders it, and Ctrl+Delete removes it. Direct comma-separated
selectors remain available for endpoints/models that are not cached. On the key row, Enter
loads the existing chain for `default`, a helper role, `provider/model`, or `provider/*`.

The preview uses runtime precedence and selector resolution, including suppression of unsafe
substitution by exact-model privacy policies. Choose Global or Project before review; Project
overrides continue to win over a Global save. Only the edited chain key is changed in the chosen
layer. Trigger and return policy are shared by all chains. The cooldown is still the server's
retry-after or five minutes, not a new configurable timer.

Nothing is saved until **Review changes** and the explicit `y` confirmation. Review warns that
conversation/source/tool data may reach a different provider with different pricing and retention.
No provider requests, credential tests, downloads or model discovery run here. Escape returns
from review to the draft, then closes without saving. Concurrent settings changes reject a stale
review; close and reopen to refresh it. Saves follow existing request-boundary adoption, never
switching a request already in flight.

Cross-provider fallback and context-promotion selectors resolve the configured provider's endpoint,
authentication variable, protocol, headers and URL templates before using bundled defaults. A
private gateway configured for a provider is not replaced with that provider's public API URL.

Example:

```toml
[retry.fallback_chains]
default = ["openai/gpt-4o-mini", "@small"]
task = ["openrouter/anthropic/claude-sonnet-4-5"]
"openai/*" = ["gpt-4o-mini"]    # any openai model without a more specific chain
"anthropic/claude-sonnet-4-5" = ["openrouter/anthropic/claude-sonnet-4-5"]

[model_roles]
small = { provider = "openai", model = "gpt-4o-mini" }
task = { provider = "anthropic", model = "claude-sonnet-4-5", thinking = "medium" }
```

Each assigned role may set its own `thinking` level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max` (`on` is not accepted; `auto` applies only to the main model). The level is applied to every request made by that role, including subagents that use it, through the role model's catalog reasoning metadata or its `[model_profiles]` entry. Without `thinking`, the role uses the model's default effort. Set it from the TUI with `/effort <role> <level>`, and clear it with `/effort <role> default`; the role must be assigned in `/roles` first. A level the role model does not support is rejected when you set it. If a hand-edited level cannot be applied, the role keeps the model default. A `@role:level` suffix on a subagent route still overrides the role level for that one worker.

Thinking overrides also apply when an auxiliary request reuses the loaded main backend (including
same-model native MLX roles and `assistance.local.side_models = "reuse_main"`). In that case the
level is resolved against the loaded main model's reasoning controls, without swapping weights.
Local phase hints do not overwrite explicit role thinking, and pending main-request controls are
restored after the auxiliary call.

### Context promotion

`[compaction.promotion]` is off by default. When enabled, a conversation that is about to be compacted (over the compaction trigger) or that the provider rejects for context overflow first switches to a configured larger-window model, and compacts only if no target qualifies.

```toml
[compaction.promotion]
enabled = true

[compaction.promotion.targets]
default = ["@long"]                      # main session; ordered list, first qualifying target wins
"openai/gpt-4o-mini" = ["openai/gpt-4o"] # exact provider/model
"anthropic/*" = ["claude-long"]          # any anthropic model without a more specific key
```

Keys resolve like fallback chains (`default`, then exact `provider/model`, then `provider/*`; only the first match is used) and targets use the same selector forms (bare model on the same provider, `provider/model`, `@role`); the resolver is shared with `retry.fallback_chains`. A target is used only when it differs from the live model, its context window is strictly larger than the current one, it can be constructed (credentials resolve), and the current request plus the output reserve fits its prompt budget. A threshold promotion must also raise the history budget. Hermetic sessions never promote. Lists must be non-empty, at most 32 entries each, at most 128 keys.

A promotion is not a fallback: it has no cooldown and nothing returns to the earlier model. It lasts until you switch models (`/model`), which resets the route. It replaces an active fallback; a later fallback fails over from the promoted model and returns to it. After an overflow, the rejected request is retried on the promoted model. Promotion is announced as a model event, and the promoted route is session-only: a rebuilt or resumed session starts on the configured model again.

Model capabilities and supported reasoning levels come from the bundled catalog when known, with explicit operator profiles remaining authoritative. Catalog context windows are used when available; otherwise remote requests use a conservative 32768-token capacity. Output budgets remain separately bounded rather than reserving the entire context window.

## Model roles in the picker

The model picker (`/roles`) assigns the shared chat/helper roles; `/improve models` also exposes learning controls. Each row shows its assigned model or fallback identity, not proof that a consumer is enabled or has run. Unassigned roles use main except `decision` (small, then main), `memory` (required for model-backed memory), and `embedding` (lexical recall when unset). Laya checkpoints and vision tool models are feature settings, not role rows. Clearing a role removes its explicit assignment.
Missing memory model assignments do not prevent Dial from opening: the selected memory backend stays inactive with a setup error. Assign its required roles in /roles, then restart Dial to activate memory. No backend or chat-model fallback is silently selected.

In the picker, `Tab` switches between the sidebar and list; `Enter` or `Right` enters the selected list. In **Roles**, highlight a role and press `Enter` to choose its model, or `x` / `Delete` to clear its assignment (main cannot be cleared). Role selection starts with a fresh search and kind filter; `Esc` returns to the same role. `Ctrl+A` on a model opens the role-assignment chooser.

| Role | Used for | Falls back to |
|---|---|---|
| `main` | Conversation and coding agent | Always assigned (the session model) |
| `task` | Implementation tasks and workers | For task-tool delegation: `coach`, then `small`, then `main` |
| `small` | Compaction, commit messages (all tiers), architecture synthesis, solve patch-search workers; preferred when assigned for scout and custom read-only subagents | `main` for maintenance; task-tool subagents follow `task` when `small` is unassigned |
| `decision` | Typed judgments for automatic thinking, judged rules, and busy-message routing; chat model or the Laya engine | `small`, then `main` |
| `coach` | Planning, escalation, advisor/completion review, feedback proposals, experience teacher, and edit repair | Plan mode: `main`; planner subagents: `task`, `small`, `main`; other consumers retain their activation gates |
| `tester` | Reviewer subagent (including `/security` scan batches and finding validation), test-runner subagent and review-workflow model calls | Task-tool reviewer/test-runner: `coach`, `task`, `small`, then `main`; other review workflows: `main` |
| `memory` | Memory extraction and consolidation (`local`) and sharpshooter model calls, including sharpshooter decision memory (`memory.sharpshooter.model_role`). Required when a model-calling memory backend is enabled | No chat fallback for model-calling memory; backend stays inactive until assigned |
| `embedding` | Semantic memory recall through an embedding-capable endpoint | Lexical recall |
| `image` | Image generation through `image_gen`, including image models served by MLX-Serve | Try `main`, then shared configured image profiles and keyed provider defaults. ChatGPT may offer an unverified hosted-image attempt when metadata is absent. Explicit `[image]` overrides still win. |
| `turn_drafter` | Experimental turn optimizer: rewrites each user message into a structured prompt before the main model sees it. Listed in /roles only while `experimental.turn_optimizer` is on (see [agent-loop](agent-loop.md#turn-optimizer-experimental)) | `main` (a separate request with the drafter's own system prompt) |

### Task-tool routing and deliberate overrides

Delegated jobs choose the first **explicitly assigned** role in the following purpose-specific chains:

| Task type | Default model-role chain |
|---|---|
| Implementation worker | `task` → `coach` → `small` → `main` |
| Scout / custom read-only | `small` → `task` → `main` |
| Reviewer / test-runner | `tester` → `coach` → `task` → `small` → `main` |
| Planner | `coach` → `task` → `small` → `main` |

Main is the last resort when no helper in that chain is assigned. Dial does not select an unconfigured catalogue model or provider. These are missing-assignment defaults, not provider-error retry chains: they do not silently restart a job on another model after work has begun. They apply only to task-tool delegation; unassigned maintenance roles retain their existing behavior.

Explicit operator agent definitions and an explicit per-call `model_profile` are deliberate overrides of this routing. They are not silently replaced by the preferred helper role or by `task`.

## Unified decision role

The `decision` role picks one answer from a fixed list, usually as `{"choice": "..."}`. It uses `[model_roles.decision]` when assigned, otherwise small and then main. Automatic-thinking answers are cached for 30 days in SQLite under Dial's user directory (`cache/judgment.db`; override with `DIAL_JUDGE_CACHE_DB`); judged-rule answers have a seven-day TTL. The cache retains 5000 recent rows and keys model identity, question, choices, and context. Busy-message routing has its own bounded request path.

To use Laya, select its typed-decision entry in `/roles`, or configure:

```toml
[model_roles.decision]
provider = "laya"
model = "decision"

[laya]
checkpoint = "/path/to/checkpoint" # optional when a checkpoint is already installed
```

`dial laya install --checkpoint PATH` saves the checkpoint under Laya settings. The alias `laya/decision` is not a chat-model download. Laya judgments do not require `/laya on`, which controls retrieval advice only. A cold service start can delay the first decision; cached Laya answers include checkpoint identity.

`thinking = "auto"` (`--thinking auto` or `/effort auto`) makes one decision per new user prompt, clamped to the main model's supported efforts. It is not repeated at every tool step. Failed or invalid judgments retain default effort with a diagnostic; hermetic agents do not judge. `portable_rules.judge` controls judged-rule calls: auto requires an explicitly assigned decision role, on permits its small/main fallbacks, and off disables them.

Plan mode and planner agents remain available without a planner model role. Plan mode uses an explicitly assigned coach or main; `--plan <model>` supplies a launch-only override without changing the coach assignment. Planner subagents use the chain above. Grounding and image search remain opt-in via their [vision feature settings](model-vision.md).
