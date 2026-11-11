//! Model setting declarations. Defaults derive from typed configuration fields.
use super::*;
const CACHE_WARM: EnumValues = &[
    ("off", "Off"),
    ("streaming", "While streaming"),
    ("idle", "Streaming and idle"),
];
pub(super) static MODEL: &[SettingDef] = &[
    field!(
        "codebase.context_max_bytes",
        codebase.context_max_bytes,
        MO,
        "Project understanding",
        "Context budget (bytes)",
        "Maximum saved project context added to a user turn; 0 means unlimited, subject to model context capacity.",
        Kind::Int {
            min: 0,
            max: 24000,
            step: 1024
        },
        true
    ),
    field!(
        "context_window",
        context_limit,
        MO,
        "Context",
        "Current model context",
        "Auto follows this model's capacity and the cloud/local default. Enter auto or a token cap. Model changes reset this override to Auto; smaller loaded server limits still apply.",
        Kind::AutoInt {
            min: 2048,
            max: crate::llm::model_config::MAX_CONTEXT_WINDOW as i64,
            step: 1024
        },
    ),
    field!(
        "cloud_context_window",
        cloud_context_window,
        MO,
        "Context",
        "Cloud context default",
        "Auto uses each cloud model's reported capacity. Enter a token cap to compact cloud history earlier; a numeric current-model override takes precedence.",
        Kind::AutoInt {
            min: 2048,
            max: crate::llm::model_config::MAX_CONTEXT_WINDOW as i64,
            step: 1024
        },
    ),
    field!(
        "local_context_window",
        local_context_window,
        MO,
        "Context",
        "Local context default",
        "Auto uses downloaded model metadata or the local server's loaded capacity. Enter a token cap for local models and local/LAN endpoints. This budgets history without preallocating model memory.",
        Kind::AutoInt {
            min: 2048,
            max: crate::llm::model_config::MAX_CONTEXT_WINDOW as i64,
            step: 1024
        },
    ),
    field!(
        "service_tier",
        service_tier,
        MO,
        "Requests",
        "Service tier",
        "Explicit provider request tier. none sends no tier; an explicit tier takes precedence over fast and slow mode. Each provider sends only the tiers it accepts.",
        Kind::Enum(SERVICE_TIER),
    ),
    field!(
        "fast_mode",
        fast_mode,
        MO,
        "Requests",
        "Fast mode",
        "Request priority processing (/fast). Ignored when an explicit service tier is set.",
        Kind::Bool,
    ),
    field!(
        "slow_mode",
        slow_mode,
        MO,
        "Requests",
        "Slow mode",
        "Request flex processing (/slow). Ignored when fast mode or an explicit service tier is set.",
        Kind::Bool,
    ),
    field!(
        "extended_context",
        extended_context,
        MO,
        "Requests",
        "Extended context",
        "Request the extended context window where the provider offers one (Anthropic API key only).",
        Kind::Bool,
    ),
    field!(
        "thinking_auto",
        thinking_auto,
        MO,
        "Requests",
        "Auto thinking",
        "Judge each new prompt's reasoning effort (off to high) once per user turn, clamped to the model's declared efforts. Falls back to the model default when no judge is usable.",
        Kind::Bool,
    ),
    field!(
        "harness.temperature",
        harness.temperature,
        MO,
        "Roles & Sampling",
        "Temperature",
        "Sampling temperature. An explicitly configured value wins, including 0.2; when unset, local hosts use their own API defaults. Select harness sampling to use the admitted harness instead.",
        Kind::Float {
            min: 0.0,
            max: 2.0,
            step: 0.1
        },
    ),
    field!(
        "harness.max_tokens",
        harness.max_tokens,
        MO,
        "Roles & Sampling",
        "Max output tokens",
        "Output limit per model call; 0 means no operator cap (local hosts choose their API default). Shared by reasoning and the answer; explicit model-profile limits still apply.",
        Kind::Int {
            min: 0,
            max: 8192,
            step: 256
        },
    ),
    field!(
        "harness.max_steps",
        harness.max_steps,
        MO,
        "Roles & Sampling",
        "Max steps per turn",
        "Model steps allowed before a turn stops; 0 means unlimited, including automatic repair. Explicit settings survive harness changes.",
        Kind::Int {
            min: 0,
            max: 64,
            step: 1
        },
    ),
    field!(
        "worker_timeout_secs",
        worker_timeout_secs,
        MO,
        "Local worker",
        "Worker timeout (s)",
        "Deadline for local workers and local model silence. 0 means unlimited; cancellation remains available.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 60
        },
        true
    ),
    field!(
        "improve.teach",
        improve.teach,
        MO,
        "Learning",
        "Teach this model",
        "Local models only (MLX, Ollama, LM Studio). Turns that prove good become training examples: passing behavioral checks, committing the changes, or Alt+Up. /undo and Alt+Down mark a bad turn. The latest version admitted by /improve train serves your sessions. Hosted models are never recorded.",
        Kind::Bool,
    ),
    field!(
        "remote_model_idle_timeout_secs",
        remote_model_idle_timeout_secs,
        MO,
        "Roles & Sampling",
        "Remote idle timeout (s)",
        "Remote model stream silence limit: waits for the first event and for silence between events. Defaults to 60 s, so a stalled connection fails and retries. 0 means unlimited. Keep-alives do not reset a positive limit, except while a tool call is open (Anthropic may stream only keep-alives while it writes a large call), for at most 10 minutes after the last real progress. Local model servers are not limited.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 30
        },
        true
    ),
    field!(
        "reasoning_guard.enabled",
        reasoning_guard.enabled,
        MO,
        "Roles & Sampling",
        "Reasoning gate",
        "Intercept long reasoning before answer text or tool arguments begin. Allow one action-focused recovery per turn, then stop incomplete. Turning this off removes only the time deadline; the reasoning byte cap always applies. Harness improvement cannot raise these limits.",
        Kind::Bool,
        true
    ),
    field!(
        "reasoning_guard.max_pre_action_secs",
        reasoning_guard.max_pre_action_secs,
        MO,
        "Roles & Sampling",
        "Before first output (s)",
        "Maximum pre-action wait; 0 means unlimited. Reasoning events do not reset a positive deadline.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 10
        },
        true
    ),
    field!(
        "reasoning_guard.max_reasoning_bytes",
        reasoning_guard.max_reasoning_bytes,
        MO,
        "Roles & Sampling",
        "Before first output (bytes)",
        "Maximum streamed reasoning bytes throughout a reply; 0 means unlimited. This is not a hidden-token estimate.",
        Kind::Int {
            min: 0,
            max: 1048576,
            step: 1024
        },
        true
    ),
    field!(
        "reasoning_guard.require_plan",
        reasoning_guard.require_plan,
        MO,
        "Roles & Sampling",
        "Require todo plan after gate",
        "Opt in to a todo-only retry when the reasoning gate intercepts a generation and no todo is open. Off by default so recovery can resume the next concrete action without replanning.",
        Kind::Bool,
        true
    ),
    field!(
        "retry.max_attempts",
        retry.max_attempts,
        MO,
        "Roles & Sampling",
        "Provider retry attempts",
        "Attempts per model request (including the first) for transient failures, throttles and incomplete streams.",
        Kind::Int {
            min: 1,
            max: 50,
            step: 1
        },
        true
    ),
    field!(
        "retry.max_delay_secs",
        retry.max_delay_secs,
        MO,
        "Roles & Sampling",
        "Longest retry wait (s)",
        "Longest single server-requested or backoff wait honoured; a longer retry-after ends the request.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 10
        },
        true
    ),
    field!(
        "retry.wait_for_quota_reset",
        retry.wait_for_quota_reset,
        MO,
        "Roles & Sampling",
        "Wait for quota reset",
        "When a usage quota is exhausted, wait for its reset (within the longest retry wait) instead of failing fast.",
        Kind::Bool,
        true
    ),
    field!(
        "retry.rotate_accounts",
        retry.rotate_accounts,
        MO,
        "Roles & Sampling",
        "Rotate subscription accounts",
        "Retry once with another signed-in, policy-allowed ChatGPT account when the pinned one stays rejected after a refresh or exhausts its quota.",
        Kind::Bool,
        true
    ),
    field!(
        "compute_locks",
        compute_locks,
        MO,
        "Local worker",
        "Compute locks",
        "Coordinate background training and maintenance with sessions. Sessions with local models share compute and never wait on each other; background work waits for them and yields to coding. Turn off to run background work concurrently; it may exhaust RAM/GPU memory. Pause and stop controls still apply. Laya never requires this lock.",
        Kind::Bool,
    ),
    field!(
        "efficiency.history_token_cap",
        efficiency.history_token_cap,
        MO,
        "Roles & Sampling",
        "History token ceiling",
        "Window that compaction and pruning plan against when the model's context window is larger; 0 uses the full window. Every step resends the history, so a 1M-token model would otherwise carry requests of several hundred thousand tokens. The default compacts near 223k. Applies when the efficiency policy is on.",
        Kind::Int {
            min: 0,
            max: 2000000,
            step: 8192
        },
        true
    ),
    field!(
        "efficiency.task_packet",
        efficiency.task_packet,
        MO,
        "Roles & Sampling",
        "Task evidence packet",
        "Append a transient packet with the active todo, recorded findings and constraints to every request while a task is in progress. Off sends only a one-line task focus.",
        Kind::Bool,
        true
    ),
    field!(
        "efficiency.batch_result_percent",
        efficiency.batch_result_percent,
        MO,
        "Roles & Sampling",
        "Reply result budget (%)",
        "Share of the remaining context window that the tool results of one reply may fill together. The largest successful results are clipped first and keep their full text as an artifact; errors and edits are never clipped. 0 turns the budget off.",
        Kind::Int {
            min: 0,
            max: 80,
            step: 5
        },
        true
    ),
    field!(
        "codebase.enabled",
        codebase.enabled,
        MO,
        "Project understanding",
        "Saved codebase context",
        "Cache source outlines and reusable architectural findings per project across sessions. Refresh changed sources and retrieve only relevant bounded context each turn. Stored outside the project; disabled in sealed evaluations.",
        Kind::Bool,
        true
    ),
    field!(
        "laya.enabled",
        laya.enabled,
        MO,
        "Laya",
        "Decision advice",
        "Use Dial's independent typed-decision service to suggest a retrieval scope before each prompt. Advisory only; does not write summaries or certify tests. /laya status shows checkpoint and results.",
        Kind::Bool,
        true
    ),
    field!(
        "laya.auto_start",
        laya.auto_start,
        MO,
        "Laya",
        "Start managed service",
        "Start Dial's installed service when its local port is stopped. Reuse ready/loading Dial services. The managed service shuts down after inactivity.",
        Kind::Bool,
        true
    ),
    field!(
        "laya.endpoint",
        laya.endpoint,
        MO,
        "Laya",
        "Local service",
        "Dial Laya endpoint, including port. Default port 8002 is separate from other applications. Requires a Dial service identity.",
        Kind::Text,
        true
    ),
    field!(
        "laya.timeout_secs",
        laya.timeout_secs,
        MO,
        "Laya",
        "Request timeout (s)",
        "Local service request deadline. 0 means unlimited; cancellation remains available.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 1
        },
        true
    ),
    def!(
        "laya.checkpoint",
        MO,
        "Laya",
        "Decision checkpoint",
        "Optional local path or Hugging Face repository for the Laya service. Delete uses the installed checkpoint; the decision role selects Laya or a chat backend independently.",
        Kind::Text
    ),
    field!(
        "codebase.synthesize_architecture",
        codebase.synthesize_architecture,
        MO,
        "Project understanding",
        "Architecture synthesis",
        "After successful turns, use the architecture model to maintain concise architecture observations from source evidence; citations are retained when provided. One bounded call per source state, run before the turn returns. /understanding build runs it manually.",
        Kind::Bool,
        true
    ),
    field!(
        "codebase.ambient_context",
        codebase.ambient_context,
        MO,
        "Project understanding",
        "Ambient codebase context",
        "Attach a bounded brief of fresh saved architecture notes, findings and source outlines relevant to each user message (no extra model request; stale findings are only named). Off by default; retrieval is then left to the loadable codebase_memory tool.",
        Kind::Bool,
        true
    ),
    field!(
        "codebase.history",
        codebase.history,
        MO,
        "Project understanding",
        "Code change history",
        "Record the diff each turn makes to each project file, with the git branch and prompt, so code_history and /code-history can search why and when code changed across sessions. No model calls. Changes made outside Dial are not recorded.",
        Kind::Bool,
        true
    ),
    field!(
        "codebase.architecture_model",
        codebase.architecture_model,
        MO,
        "Project understanding",
        "Architecture model",
        "Local MLX model for saved architecture summaries. Default: Qwen2.5-3B Instruct 4-bit, a compact summarizer. Requires the MLX runtime and model weights. Blank follows the main model; an explicit Small role in Models & roles takes precedence.",
        Kind::Choice(ARCHITECTURE_MODELS),
        true
    ),
    field!(
        "retry.fallback_on",
        retry.fallback_on,
        MO,
        "Roles & Sampling",
        "Fallback trigger",
        "Which failures move a request to its fallback chain: any recoverable failure, only usage limits (throttling and quota), or everything except usage limits.",
        Kind::Enum(FALLBACK_ON),
        true
    ),
    field!(
        "retry.fallback_revert",
        retry.fallback_revert,
        MO,
        "Roles & Sampling",
        "Fallback revert",
        "When the main session returns to its primary model after a fallback. Cooldown expiry returns at the first request after the fallback's cooldown; never stays on the fallback until you switch models. A request in flight is never switched.",
        Kind::Enum(FALLBACK_REVERT),
        true
    ),
    field!(
        "cache.warm",
        cache.warm,
        MO,
        "Cache warming",
        "Prompt cache warming",
        "Replays the settled prefix shortly before the provider's cache lifetime ends, when the saved cache read is worth more than the replay. Off never replays. Streaming replays only while a run is active. Idle also replays after the run ends, for up to an hour. Applies to native Anthropic endpoints only.",
        Kind::Enum(CACHE_WARM)
    ),
    field!(
        "retry.max_elapsed_secs",
        retry.max_elapsed_secs,
        MO,
        "Roles & Sampling",
        "Retry elapsed budget (s)",
        "Stop starting or waiting for retries after this elapsed request time; 0 is unlimited. A successful first attempt keeps its model timeout.",
        Kind::Int {
            min: 0,
            max: 3600,
            step: 10
        },
        true
    ),
];

pub(super) static EFFICIENCY: &[SettingDef] = &[
    field!(
        "efficiency.preserve_task_inputs",
        efficiency.preserve_task_inputs,
        MO,
        "Roles & Sampling",
        "Preserve task inputs",
        "Capture bounded root-level inputs before an unattended task starts (off by default). Independent bytes are retained outside the workspace; hidden, credential-like, oversized and linked sources are skipped.",
        Kind::Bool,
        true
    ),
    field!(
        "efficiency.evidence_stall_rounds",
        efficiency.evidence_stall_rounds,
        MO,
        "Roles & Sampling",
        "Evidence stall limit",
        "Unattended work rounds without new independent observations or changed check verdicts before stopping blocked. A hypothesis checkpoint occurs halfway. 0 disables the guard for an ablation.",
        Kind::Int {
            min: 0,
            max: 256,
            step: 4
        },
        true
    ),
    field!(
        "efficiency.small_case_guard",
        efficiency.small_case_guard,
        MO,
        "Roles & Sampling",
        "Small-case checkpoint",
        "Before the first large unchecked write, queue one controller notice requesting a tiny independent known-answer case before further generation. Does not discard the admitted write.",
        Kind::Bool,
        true
    ),
];

pub(super) static GROUPS: &[&[SettingDef]] = &[MODEL, EFFICIENCY];
