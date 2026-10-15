# Optional learning and harness improvement

Coding and learning have separate lifecycles. Normal terminal, print, RPC and ACP launches use configured model/harness settings and do not record training trajectories, apply learned versions, start Laya/automatic learned-skill capture, generate project-learning summaries, run startup model evaluations, monitor learning adapters, or queue improvement at startup/exit. Legacy automatic-learning preferences remain on disk and cannot opt a coding session back into this work.

Run `dial improve status` to inspect learning, `dial improve cycle` for an explicit gated cycle, or `dial improve train` for explicit training. Inside the terminal, `/improve settings` and `/improve models` expose the separate learning controls; `/improve jobs` and `/improve stop <state_id>` manage explicitly launched work. These operations retain compute, power, budget and evaluation gates. Learning workers yield to coding at their existing safe handoff boundaries.

Existing verified trajectories remain available to the explicit learning pipeline. New ordinary coding chats stay in the session journal for resume, review and undo, without becoming training examples. To evaluate task/model performance, run the explicit benchmark and improvement tools.

## What a cycle does

`/improve jobs` shows each model's worker status and log alongside the shared compute lock.
`/improve stop <state_id>` requests a safe stop for the model ID shown there. Updated training
subprocesses cancel promptly; inference/evaluation finishes its current case before stopping.
A stop also pauses future cycles until `dial improve enable` is run for that model. Workers
started with older binaries cannot acknowledge stop requests; the result explicitly reports this.
A recorded `running` state with `control_busy: false` is stale, not evidence of active training.
Compute can be held by another model, project, interactive session, or capability verification.

`/improve` shows the remaining cycle and LoRA cooldowns separately, new-turn prerequisites,
the active stage and elapsed cycle time. The learning views show stage progress
(including self-play task and evaluation-check counts). LoRA training reports its planned iteration
count while waiting for the worker; it does not yet stream per-iteration progress or predict a finish
time. Eligibility also depends on verified training data, so an elapsed cooldown is not a training ETA.

1. **Observe and monitor explicit evaluations.** Keep evaluation tool events, repaired calls and completed evaluation conversations. The event-feature logistic probe ranks risky steps when enough externally scored trajectories exist and its held-out AUC is informative; predicted risk remains separate from ground-truth scores. Historical learning-enabled trajectories retain their human verdicts and undo rejections; normal coding journals do not create new learning trajectories. A turn that ends cleanly after its last edit, with every behavioral check run in that turn passing, is recorded automatically as `checks:behavioral` score 100; a later `/outcome` or `/undo` replaces it. `/improve` shows previously recorded verified turns and explicit evaluation results. Shell exit codes alone are telemetry, not task-success labels.
2. **Practice with external checks.** Run 24 generated file-editing, scripting, search and structured-data tasks in disposable, strictly sandboxed workspaces. Files, JSON values, answers and command results are checked deterministically. Later cycles prioritize train families with low verified pass rates, high risk scores or recent tool-error categories; gate/anchor families never enter self-play.
3. **Build training data.** Harvest successful verifier-scored trajectories plus schema-invalid tool attempts that were later corrected and executed without error. A bounded preference pair keeps the malformed assistant response as rejected and the verified correction as chosen; other raw failures are not preference labels. Conversations remain in one train/validation split, and held-out prompts are excluded. `learn_from_sessions = false` excludes future harvesting of live conversations and repairs.
   Reasoning models retain `reasoning_content` on the final assistant target, not earlier turns. The worker uses the model's native reasoning template when available, otherwise its thinking markers; models without either omit that field. Training, preference scoring and likelihood screening share the same token boundary and target mask.
4. **Train locally.** An isolated feedback-model invocation proposes harness changes. Bounded mutations explore prompts, notes, tool selection, temperature, step budgets and edit mode. For supported local MLX models, LoRA resumes from the active adapter on up to 32 new verified records plus 16 accepted replay records; validation is capped at 32. A small reference-free preference pass adds chosen SFT loss plus a 0.1-weighted pairwise logistic ranking loss on repair pairs. The base model stays frozen; rejected candidates do not advance the training watermark.
5. **Validate before activation.** Evaluate against fresh primary and general tasks, plus a frozen anchor suite. The primary (`tools`) suite is adaptive per model within a cycle: the baseline is scored at tier 0, and if it passes at least 90% of its cases (integer arithmetic, so 89.9% stays and 90.0% advances), the suite escalates to tier 1 and the baseline is scored again, then to tier 2 (the cap). The candidate runs on the final tier against the same baseline. Tier 1 adds five gate-only hard families (`refactor_behavior`, `search_then_edit`, `config_chain`, `bugfix_tests`, `multi_file_chain`) to the gate slots; tier 2 draws only from those hard families. Tier 0 excludes them, so its mixture is unchanged. The chosen tier is recorded on the `tools` score in the admission report. Escalation is not persisted across cycles and applies only to auto-generated suites; manual `[gate]` suites are not tiered. Admission requires primary improvement, enough paired wins, an exact one-sided sign test (adjusted for the cycle's candidate count), and no suite dropping over one percentage point. The anchor also limits cumulative drift from the original version. Private case contents and failed evaluator details never go to the feedback model.
6. **Serve or reject.** At most one candidate is admitted per cycle, with an atomic SQLite baseline check. Rejected candidates never become active. New turns pick up admitted versions; adapter changes restart the local server. A live monitor rolls back when enough external verdicts show a configured drop from the predecessor.

LoRA is enabled by default but requires a supported checkpoint, **24 examples** in the bounded delta/replay batch, a nonempty validation split and a **6-hour training interval**. Training defaults to **32 iterations**. A candidate must still pass the full sealed gate before activation. Data-poor cycles can still evaluate harness changes; no gain is guaranteed.

**Gate measurement under infrastructure failure.** Each gate case is measured as a baseline/candidate pair. A transient provider or transport failure (rate limit, 5xx, timeout, lost connection, model load failure) is retried twice, after 2 s and 8 s. If it persists, the whole pair is excluded from both versions, so they are always compared on the same cases; the admission reason records how many pairs were excluded and the failure category (never prompts or output). Failures caused by the model's own output (thinking loops, rejected tool-call JSON, context overflow) still count as unsolved for the candidate. Operator stops, budgets, authentication or quota errors, and unrecognized failures abort the evaluation as before. If more than 20% of a private suite is excluded, the evaluation does not complete and nothing is admitted. The cumulative-drift anchor is never cached from a partial measurement: if it cannot be measured, the candidate is rejected for that cycle and retried later.

**Feedback proposals.** The Feedback-Agent uses the `coach` role (assigned in /roles). Its reply is parsed leniently: repaired JSON of the whole reply, then the outermost JSON object in it. If that fails, it gets exactly one stricter retry asking for only the JSON object. If no proposal can be recovered, the cycle falls back to deterministic mutations and records `feedback_agent_failed`.

## Fast weight candidates without long training runs

Local MLX cycles can construct and filter weight candidates without running gradient descent:

- **Steering search:** extract contrast directions at roughly 25/40/50/60/75% of model depth and compare five strengths using four-fold held-out chosen/rejected likelihood margins. Strength is scaled by the contrast magnitude rather than fixed at 0.5. The selected vector still needs the existing response-contract admission check.
- **Closed-form adapter:** fit a rank-1 LoRA by ridge least squares to approximate an admitted steering vector. Narrow dense MLP output projections receive the change at the block output; wider or expert layouts use the next block's attention output projection, or the current attention projection at the last block. That fallback is an approximation, not identical activation steering. This produces an ordinary, resumable adapter without optimizer iterations.
- **Adapter interpolation:** when a trained candidate and parent adapter exist, compare 25/50/75% candidate mixtures. Rank concatenation represents the exact weighted sum of their weight deltas; averaging LoRA factors separately would not. Only the highest-likelihood mixture proceeds as a soup candidate.
- **Likelihood pre-screen:** score weight candidates on up to 64 validation examples before expensive agent rollouts. A candidate must improve mean target log-probability, win on at least half the paired examples, and not lower an available preference margin. Survivors run through the full correctness/drift gate; likelihood alone never activates an adapter. `/improve` cycle reports retain `prescreen`, `soup` and `steering_lora` evidence.

These paths reduce candidate construction and rejection costs, not all cycle costs: self-play, harness-only candidates and the full gate for surviving adapters still run agent tasks. No improvement is guaranteed, and large-model scoring or ridge solves can still be expensive.

This is not online learning on every prompt: normal requests do not change model weights. The cycle
may train a separate LoRA adapter for a supported local MLX model from accumulated verified examples.
The adapter only becomes active after the external gate admits it; rejected candidates do not alter
the serving model. Hosted endpoints can improve the harness, not their remote weights.

During live work, Dial may attach up to three matching repair hints from the same project to the user message they match. They are stored with that message, never placed in the system prompt, so the cached prompt prefix does not change from turn to turn. Hints show only tool names, argument field names and failure categories—not old argument values—and never bypass current validation or approval.
Dial also retrieves up to two similar externally verified live tasks as advisory examples. Their tool sequence includes tool names and argument keys, never old argument values; self-play and mismatched-project snapshots are excluded. These examples are attached to the user message, not the cached system prefix, and are not used during hermetic evaluation.

The MLX-LM server adapter preserves malformed model tool calls as explicitly invalid, non-executable calls rather than allowing the pinned upstream formatter to crash on missing arguments. These errors remain visible to the agent and the learning pipeline. Its prompt (KV prefix) cache is bounded to a quarter of physical memory (`--prompt-cache-bytes`); mlx-lm's default holds up to ten caches with no byte limit, which can push weights into swap on unified memory.

**Schema-constrained tool calls.** For local MLX-LM models, Dial's server constrains decoding only inside tool-call spans, for the tool-parser formats that mlx-lm ships with: `json_tools` (`<tool_call>{"name": …, "arguments": …}</tool_call>`, e.g. Qwen2.5/Qwen3/Hermes templates), `gemma4` (`<|tool_call>call:NAME{…}<tool_call|>`, strings in `<|"|>`), `function_gemma` (`<start_function_call>call:NAME{…}<end_function_call>`, strings in `<escape>`, and `}` is not allowed inside a string), `glm47` (`<tool_call>NAME<arg_key>…</arg_key><arg_value>…</arg_value>…</tool_call>`, where string values are checked only against enums and other values as JSON) and `mistral` (`[TOOL_CALLS]` followed by a JSON array of calls or `NAME[ARGS]{…}`, ending when its JSON is complete). Each sampled token must keep the call a valid prefix: syntax, a declared tool name, declared argument keys (unless `additionalProperties` allows extras), required keys before the object closes, value types, string enums, and array item types. The model's own distribution is sampled first; only a rejected token is resampled among valid alternatives, so calls that were already valid come out unchanged. Free text and thinking spans are not touched. `anyOf`/`oneOf`/`$ref` and numeric or length bounds are checked for syntax only, and Rust-side validation still has the final say. A dead end releases the constraint for that call, never stalling generation. It is disabled under speculative or draft-model decoding, distributed decoding, MLX-VLM, other tool formats (e.g. Qwen3-Coder XML) and external servers. Because constrained calls are always executable, this reduces the malformed-call repair pairs available for preference training.

```sh
dial improve jobs                   # all project models, active locks, stage and recent log
dial improve stop <state_id>        # cancel training / stop after current case; pause future cycles
dial improve status                 # scheduler state, last cycle, scores, probe and layer sensitivity
dial improve versions
dial improve disable                # pause pending/future automatic cycles; a running cycle finishes
dial improve enable
dial improve cycle                  # optional: run a complete cycle now
dial improve train                  # optional: request a gated LoRA candidate now
dial improve diagnose
dial improve harvest
dial improve propose proposal.json
dial improve rollback
dial improve outcome 42 --score 100 --source human-review
```

## Verified local-model workflows

These features improve the harness around a model; they do not imply a measured increase in model
quality merely because training ran.

- **Executable repair evidence.** In recorded live sessions, the same declared check must fail, a
  checkpointed file must change, and the check must subsequently pass. Dial retains the command,
  objective, exit results, immutable output, and before/after source bytes with hashes. Sources and
  outputs are retrievable through `artifact_read`, including after undo. Related records are retrieved
  within the same project; exact failed-state applicability is checked with a bounded hash scan.
  An unchanged flaky pass, timeout, missing snapshot, or changed command cannot establish a repair.
  Changed tests or environment can still explain a pass: this evidence never sets a trajectory success
  score or bypasses the training admission gate.
- **Measured workflow candidates.** Recurring tool-form and investigation failures propose protocol
  discipline, alternate edit-format, or bounded-investigation harness profiles. Profiles enter the
  existing held-out correctness/drift gate. Paired measurements additionally reject newly lost cases
  and hidden regressions in tool calls, protocol errors, chatter, or generation rounds. Candidate
  rationale, source counts, measurements and model identity persist with the version; existing
  activation/rollback remains authoritative. An efficiency-only gain cannot bypass correctness admission.
- **Targeted practice.** Recurring failure categories prioritize generated configuration-precedence,
  empty-search-versus-error, and behavioral-boundary exercises. Separate held-out contracts test
  transfer to null/clearing rules, search recovery and semantic boundaries. Synthetic variants use
  aggregate categories, not copied project source, user prompts or sealed gate answers.
- **Per-todo evidence packets.** An active task gets a bounded context packet containing its question,
  recorded findings, verification objective and evidence references. At task transitions, eligible old
  successful observation bodies are replaced by artifact references. All user messages, call/result
  pairing, mutation/error results and the most recent twelve messages remain intact. The session is
  append-only; packets stay stable while the task state is unchanged and never modify the system prefix.

Behavioral evidence is opt-in per configured check. Syntax/build checks remain explicitly classified
as syntax-only unless the operator identifies the command as testing behavior:

```toml
[verification]
behavioral_checks = ["regression"]

[[verification.commands]]
name = "regression"
command = "python3 -m unittest discover -s tests"
file_extensions = ["py"]
timeout_secs = 120
```

**Isolated patch search and escalation** are optional specialist tools, disabled by default. Enable
them in user/project configuration, then load `solve` or `escalate` with `tool_load`:

```toml
[solve]
enabled = true
max_candidates = 0
max_steps = 0
max_snapshot_bytes = 0

[escalation]
enabled = true
allow_remote = false
max_evidence_bytes = 12000

[model_roles.coach]
provider = "mlx"
model = "/absolute/path/to/stronger-mlx-model"
```

`solve` takes `goal`, exact relative `allowed_paths`, independent `candidates` (approach strings),
and explicit `checks` (`command`, optional `timeout_secs`). It runs bounded local workers in separate
copies, then each check in another isolated copy. Workers and checks are network-disabled and follow
approval policy. A candidate that changes other files or fails a check cannot win. Among passing
candidates, the smallest changed-byte span wins; application refuses stale source or symlinks and
preserves unrelated dirty/untracked work. Checks establish only what they actually assert, not broad
semantic correctness. Source files are never reset or checked out.

`escalate` takes only `goal`, `question`, `evidence` and `rejected_hypotheses`. It sends no automatic
repository files or conversation history. The coach role must be assigned explicitly in /roles. Local MLX
and exact loopback endpoints are allowed by default; private LAN and public endpoints both require
`escalation.allow_remote = true`. Approval remains separate. Advice is marked unverified, carries
usage/cost accounting, and must be checked locally before becoming a correctness claim. Neither a
solver win nor a stronger-model answer automatically becomes a verified training outcome.

**Automatic escalation** runs only when `[escalation].enabled = true` and the `coach` role is assigned. After `auto_after_failed_rounds` (default `3`; `0` disables) consecutive failed tool rounds in one turn, Dial asks the coach role once for unverified advice, up to `max_auto_per_turn` times per turn (default `1`). It uses the same privacy gate, limits, and cost accounting as `escalate`. Approval follows the same decision as `escalate`: `ask` prompts you first, `readonly` and plan mode refuse, and a denial skips the call without contacting the coach model. A refusal, denial, or remote-disclosure refusal appears as a skipped notice and still uses the turn's allowance, so one refusal cannot repeat every round. The question contains the turn's user goal and recent failure messages only. Advice reaches the next model turn as a transient, clearly unverified user message, never as a system instruction, and it activates nothing. Both keys appear under `/settings` → Automatic escalation.

A local main model with a hosted coach model:

```toml
[escalation]
enabled = true
allow_remote = true              # required: the coach endpoint is remote and receives the bounded question
auto_after_failed_rounds = 3
max_auto_per_turn = 1
max_evidence_bytes = 12000

[model_roles.coach]
provider = "openai"
model = "operator-model"         # replace with the exact hosted model identifier
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
```

**Actionable advisor mode** (`--advisor` or `/advisor on`) is a separate, launch-scoped opt-in.
Before accepting a completed reply or `finish(completed)`, the coach reviews the original goal,
proposed reply and a bounded ledger of recent tool results (including check failures). It must
identify specific gaps, reference supplied evidence and propose local checks—not just comment
on the answer. Supported findings return to the active tool loop for one repair opportunity;
the coach then reviews the result once more. Normal turn budgets, approvals, sandbox rules and
interrupts still apply. Blocked and needs-input responses do not trigger autonomous repairs.
Reviews share escalation's remote-disclosure gate (`escalation.allow_remote`), evidence limits,
request hooks and usage/cost accounting. `/advisor on` does not enable automatic escalation or
the `escalate` tool. Refused or malformed reviews are reported and not retried automatically.
Review outcomes and counts of local checks after repair are recorded in the turn trace;
remaining findings are shown explicitly. A coach finding—or a review with no gaps—is never
verification or verified training evidence. During-turn escalation also receives bounded recent
tool observations and asks for an alternative approach plus a discriminating local check.

## Learning from other models in this project

A local MLX model can train on work that other models (hosted or local) did in the same project.
It is opt-in and off by default. Enable it in `/settings` → Improvement → Learn from other models,
or in configuration:

```toml
[training]
learn_from = ["claude-sonnet-5-5"]   # or learn_from_all_verified = true
```

1. Run the local model through Dial's own MLX runtime (`provider = "mlx"` with a local checkpoint
   path). LM Studio, Ollama and other endpoint models cannot be trained.
2. Use the other model in this project as usual. Its sessions record trajectories in its own learning
   database; verify the good ones with `/outcome` (or `dial improve outcome <id> --score 100`).
   Its automatic improvement cycles also add self-play trajectories that a checker verified.
3. Switch back to the local model. The next improvement cycle harvests the verified records,
   trains a LoRA candidate and admits it only if it passes the gate. To request training without
   a full improvement cycle, use `/improve train` inside Dial (or `dial improve train` in a terminal).
   The in-app command queues training and evaluation in the background; follow it with
   `/improve jobs` or cancel it with `/improve stop`.

- `training.learn_from` names the source models. A name matches the model recorded in that model's
  learning database, with or without a `provider/` prefix (the database does not store the provider).
- `training.learn_from_all_verified = true` admits verified data from every other model in the
  project without listing them.
- `training.learn_from_max_percent` (0–50, default 25, validated at config load) caps the foreign share
  of each batch. When local records are below `lora_min_examples` (or `batch_size`, if larger), foreign
  records fill the batch up to that minimum. Above it, foreign records are limited to
  `local * pct / (100 - pct)`.
- `improve.learn_from_sessions = false` excludes live sessions from every model, leaving only
  self-play data.

Only externally verified records are admitted: successful trajectories with score 100 and no
`check:` source, plus tool-recovery pairs. Held-out prompts are excluded exactly as for local data,
and sealed gate data is never read. Foreign records keep their origin in the training `source`
field (`dial:learned:<origin>:<id>:<inner>`, where origin is the sibling's directory name under `~/.dial/projects/`), and
the cycle report counts them under `learned_from`. Reasoning is stripped from foreign assistant
turns, and foreign repairs never produce preference pairs. Adapters still pass the full gate before
activation.

On an endpoint model, the skip reason now names an on-disk MLX checkpoint of the same model when one
resolves, and says to train that checkpoint with `provider = "mlx"`.

## Storage, hosted endpoints and unavailable capabilities

`improvement.log`, datasets, adapter artifacts and the SQLite audit history live under the project's private state directory, outside the workspace. For custom use cases, an optional `[gate]` can point to your own independent JSONL suites; cases accept `expected`/`matcher` or a declarative `check`, and optional seeded `files`. Automatic generated tasks measure a bounded coding/tool-use curriculum, not arbitrary real-world correctness. Custom gates use their explicit point thresholds rather than the automatic curriculum's significance rule.

On hosted endpoints, the loop can improve the harness but cannot train weights; automatic evaluation and feedback use that provider and may incur API charges. Background cycles on a cloud main model stay off until `[improve] max_daily_usd` is greater than `0`. Set `[improve] enabled = false` to disable recording and scheduling entirely. Set `[training] enabled = false` to keep the harness loop but prohibit weight training.

Still capability-unavailable: expert growth, routing recalibration, attention retrofit and a hidden-state neural probe. The implemented event-feature risk probe and gradient-norm layer scoring are not implementations or benchmark reproductions of the named research papers.

## Keeping improvement off your machine's critical path

Background cycles start only when the machine is free (`[improve]`):
`idle_minutes` (default 10) since your last coding turn, `require_ac_power` (default true; macOS
`pmset`), not in Low Power Mode, not thermally throttled, and under `daily_compute_minutes`
(default `0` = unlimited), which is also enforced while a cycle runs: it pauses with state
`paused` and resumes when next due. `dial improve cycle` and `/improve start` bypass the start-time
checks. `dial improve status` shows `waiting_for`, `compute_used_today_secs` and
`last_cycle_seconds_by_phase` so you can see whether generation, training or evaluation holds the machine.
Evaluation stops early when a rejection is already certain (a suite over its regression bound, or a
primary suite that cannot reach the required gain even if every remaining case went the candidate's
way). This never changes whether a candidate is admitted, only how many cases run. Cloud main
models never wait for local compute; only local models and explicit local roles take the compute lock.

## Measured harness improvement and repository exercises

Dial bounds initial reasoning according to task complexity (15–60 seconds depending on phase, capped by the configured
reasoning limit), redirects repeated unchanged investigations, and saves completed investigation
findings with source hashes. Investigation todo findings and drop/block reasons allow up to 4,000
characters, including relevant detail and remaining uncertainty; saved codebase notes allow up to
20,000 UTF-8 bytes.
Saved findings are untrusted context: changed sources make them stale,
and replaying an old completed todo cannot make them fresh again. Error recovery hints distinguish
missing paths, malformed calls and failed edits so retries can change approach.

Improvement records tool errors, repeated investigations, interventions, tool calls, tokens and elapsed
time alongside correctness. Task profiles may select an accepted version's known-good parent for a
particular task category only after paired evaluation shows a consistent advantage without lost
correctness. Unmeasured categories retain the active version. `/improve` and `dial improve status`
show the measurements; timing is diagnostic and does not by itself establish improved correctness.

Register real repository exercises for the selected model with an explicit JSON manifest:

```json
{
  "id": "parser-boundary",
  "prompt": "Fix the parser's boundary handling while preserving valid inputs.",
  "split": "train",
  "files": ["src/parser.py"],
  "verification_files": ["checks/parser_boundary.py"],
  "command": "python3 -B checks/parser_boundary.py"
}
```

```sh
dial improve register-task exercise.json
dial improve tasks
dial improve register-task repair.json --trajectory 123
```

Registration captures immutable, explicitly listed UTF-8 source and verification files. Include every
required dependency; checks run offline in an isolated workspace. Verification files are withheld
from the initial solver workspace and restored before verification. Use separate `evaluation`
exercises for held-out checks: duplicate content and prompts cannot cross splits. Repository
exercises supplement the automatic evaluation suites; they do not replace the primary admission
gate. Each cycle replays at most four registered training tasks. Training may make one correction
after an independent failure; evaluation receives no such repair attempt.

`--trajectory` imports a retained behavioral repair, including its original failed source snapshots.
The supplied independent checks must fail before and pass after the recorded change. The manifest
must list all changed files, including deletions; changed verification files are rejected. A repair
record or a successful tool call alone never becomes a successful training label. Syntax-only checks
remain distinct from behavioral evidence.

The optional `solve` and `escalate` tools use the configuration documented under **Isolated patch
search and escalation** above. Candidate search verifies independent patches in isolated workspaces;
escalation supplies compact evidence to an explicitly configured stronger model. Both remain disabled
until configured, and stronger-model advice still requires local verification.

Use `/improve jobs` to distinguish the selected model's idle state from another model's active
compute job, and `/improve stop <state_id>` to request cancellation. LoRA support verification,
training, evaluation, activation and confirmed adapter use are separate states. An available or
verified adapter capability does not mean training has run or that inference used an adapter.

## Experimental MLX activation steering

Dial can extract a unit-length behavior vector from matched positive/negative assistant responses,
then add it to one language-decoder transformer layer during inference. This runs in Dial's own
MLX-LM text server and MLX-VLM vision server, including image requests and cached decoding.
The vision encoder stays unchanged. Vision-model extraction and evaluation use the same decoder
weights and model tokenizer for text/tool contracts; these checks do not certify image accuracy.
Hosted providers and external inference servers have no Dial activation hook. Incompatible decoder
architectures bypass the vector and report the reason through `/steering`. Automatic creation is enabled
by default as part of automatic improvement; no vector is used before it passes evaluation.
No random vector is activated automatically, and this is independent of Laya.

Automatic creation reads only train-split preference pairs matching recorded successful tool repairs.
It waits for at least eight distinct, bounded contrast prompts, sweeps candidate layers across 25–75% of model depth and strength multiples,
and tests eight separate exact tool-call contracts covering read, write, grep and glob. Candidates
must improve over the unsteered model, pass the existing repetition and latency gates, and avoid
reducing the incumbent vector's held-out score. Failed candidates preserve the active vector.
These are response-contract checks, not proof of overall coding improvement. Manual vectors are
preserved. Unchanged completed attempts have a 24-hour cooldown; new evidence can trigger a new attempt.

`/settings` → Improvement → Activation steering → **Automatic vector creation** controls both automatic
creation and use. **Enable manual vector** controls explicit vectors independently. `/steering`
shows collecting `N/8` prompts, candidate extraction/evaluation progress, rejection/admission and
actual runtime use. Jobs share the improvement cycle's compute lease, budget, stop controls and
foreground handoff. Auxiliary roles and sealed harness evaluation remain unsteered.

```sh
dial steering status
dial steering extract --pairs pairs.json --layer 14
dial steering evaluate --cases held-out.json
```

Use `-m MODEL` before `steering` to select the model. Choose a zero-based layer that actually exists;
Dial probes the model's layer count and hidden dimension. Extraction requires 8–64 distinct matched
prompts. The pairs file is a JSON array with this shape for each entry:

```json
{
  "positive": [{"role":"user","content":"Implement a typed addition function."},
               {"role":"assistant","content":"def add(a: int, b: int) -> int:\n    return a + b"}],
  "negative": [{"role":"user","content":"Implement a typed addition function."},
               {"role":"assistant","content":"def add(a, b):\n    return a + b"}]
}
```

Both sides must have identical preceding messages and different final assistant responses. Pick
examples that isolate the behavior you want. The held-out file is a separate JSON array of 8–32
cases, with distinct prompts that did not appear in extraction:

```json
{
  "messages": [{"role":"user","content":"Write a typed integer multiply function."}],
  "required": ["def multiply(", "-> int"],
  "forbidden": ["TODO"],
  "python_ast": true
}
```

Each case needs required text or an `expected_tool`. Tool cases can additionally supply OpenAI
`tools` declarations and `required_arguments`; checks require complete JSON, the expected tool
name and presence of those arguments. These are response contracts, not comprehensive tool-schema
or coding correctness checks. Generation is deterministic and capped at 128 tokens per case.

Evaluation compares baseline and steered responses in alternating order. Admission requires a
strictly better contract pass count, at least 80% passing, no tool-validity/repetition regression,
and bounded first-token latency regression. A passing result is **not full coding accuracy
certification**; inspect real coding behavior before relying on a vector. Admission is tied to
this artifact and exact strength. Model/adapter metadata changes invalidate the binding; the
fingerprint uses paths, file sizes/timestamps and selected config content, not a full hash of every
weight byte. If using LoRA, pass the same `--adapter /absolute/path` to both maintenance commands;
a base-model vector is bypassed when an adapter is loaded.

For a manually extracted vector, after evaluation open `/settings` → Improvement → Activation steering
to enable the manual vector and select
the vector name and strength. `/steering` reports the evaluation, completed/total job progress,
and live `loaded`, `used` (with forward-pass count), `bypassed` or `unconfirmed` state. CLI status
reads saved state without starting inference and cannot confirm another session's runtime use.
Changing the profile restarts the inference server and clears its prompt cache. A missing,
unadmitted or incompatible vector is bypassed so coding stays available. Auxiliary model roles
and sealed harness evaluations do not use steering.

Extraction/evaluation are explicit background jobs. They wait for idle compute, obey the existing
`/compute` global pause/resume controls, model stop flags and daily compute budget, and yield to foreground coding.
A checkpoint is saved after each extraction pair or evaluation case; interrupted work repeats only
the unfinished unit. `Ctrl+C` stops the command and its worker; rerun the same command to resume.
After an operator stop, select Start in `/compute` before resuming. Changing the dataset or model
binding starts a fresh checkpoint. Progress and private JSON vectors live under the model's
learning directory in `steering/`; no weights are changed. No checkpoint compatibility is promised
across steering format versions.

## Portable adapter and steering packs

`dial improve export <version> --out pack.tar.gz` writes a verified pack of one LoRA adapter version
(`adapter_config.json` and `adapters.safetensors`) with SHA-256 digests and a base-model fingerprint.
`dial steering export --out pack.tar.gz` does the same for the local steering vector.

`dial improve import pack.tar.gz` checks the manifest, base fingerprint, file list, sizes and digests,
then stores the adapter as a **pending candidate** on the active version. `dial improve evaluate <id>`
runs the normal local gate; nothing activates unless that gate admits it.
`dial steering import pack.tar.gz [--replace]` verifies the pack and stages its vector as a **pending**
import under `steering/pending/imported-<sha12>.json`, re-bound to this checkout with any exporter
evaluation removed. Imports never touch the active vector, and an existing pending import with the
same name is refused unless `--replace` is passed. Evaluate it locally with
`dial steering evaluate --pending <name> --cases <file>`, then make it active with
`dial steering promote <name> [--replace]`. Promotion requires a passing local evaluation of that exact
vector; without `--replace` it refuses to overwrite an existing active vector.
Packs carry weights and provenance only; exporter admission records are informational and are
re-verified on import.
