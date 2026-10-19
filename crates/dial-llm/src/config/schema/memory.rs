//! Memory setting declarations. Defaults derive from typed configuration fields.
use super::*;
pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "memory.backend",
        memory.backend,
        LE,
        "Memory",
        "Memory backend",
        "Which memory system Dial uses. Only one is active at a time. Provenance keeps facts, revisions and expiry; rollout summary builds summaries from past sessions; the service options need their own setup.",
        Kind::Enum(&[
            ("off", "Off"),
            ("provenance", "Provenance facts"),
            ("local", "Rollout summary"),
            ("hindsight", "Hindsight service"),
            ("sharpshooter", "Project decisions"),
            ("semantic", "Semantic facts")
        ]),
        false
    ),
    field!(
        "memory.autolearn",
        memory.autolearn,
        LE,
        "Memory",
        "Learn tool",
        "Enable native learning guidance when the selected backend offers learn; optional private managed skills cannot shadow authored skills.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.autolearn_auto_continue",
        memory.autolearn_auto_continue,
        LE,
        "Memory",
        "Automatic private capture",
        "After substantive normal turns, opt into a genuine owner-scoped learn-only capture turn. Never simulates a user reply or approval; aborted/plan/goal phases are excluded.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.autolearn_min_tool_calls",
        memory.autolearn_min_tool_calls,
        LE,
        "Memory",
        "Capture tool-call threshold",
        "Minimum actual completed-turn tool calls for automatic capture eligibility.",
        Kind::Int {
            min: 1,
            max: 1000,
            step: 1
        },
        false
    ),
    field!(
        "memory.context_limit",
        memory.context_limit,
        LE,
        "Memory",
        "Auto-recall result cap",
        "Prepared memory recall commits only after winning authentic user delivery. Zero uses the backend default.",
        Kind::Int {
            min: 0,
            max: 50,
            step: 1
        },
        false
    ),
    field!(
        "memory.context_max_bytes",
        memory.context_max_bytes,
        LE,
        "Memory",
        "Prompt memory size (bytes)",
        "Largest amount of memory text added to a prompt. Zero uses the backend's default limit. Memory is data, never instructions.",
        Kind::Int {
            min: 0,
            max: 24000,
            step: 256
        },
        false
    ),
    field!(
        "memory.request_max_bytes",
        memory.request_max_bytes,
        LE,
        "Memory",
        "Full request size (bytes)",
        "Largest size of the full request sent to the memory model, including its instructions. The model's own context limit still applies.",
        Kind::Int {
            min: 1024,
            max: 16 * 1024 * 1024,
            step: 1024
        },
        false
    ),
    field!(
        "memory.local.max_rollouts_per_startup",
        memory.local.max_rollouts_per_startup,
        LE,
        "Rollout memory",
        "Sessions per background run",
        "Most past sessions handled in one background pass.",
        Kind::Int {
            min: 1,
            max: 1024,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.max_rollout_age_days",
        memory.local.max_rollout_age_days,
        LE,
        "Rollout memory",
        "Session age limit (days)",
        "Skip past sessions older than this many days.",
        Kind::Int {
            min: 1,
            max: 3650,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.min_rollout_idle_hours",
        memory.local.min_rollout_idle_hours,
        LE,
        "Rollout memory",
        "Idle time before use (hours)",
        "A session is used only after it has been idle this long. The active session is never used.",
        Kind::Int {
            min: 0,
            max: 8760,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.thread_scan_limit",
        memory.local.thread_scan_limit,
        LE,
        "Rollout memory",
        "Saved session scan limit",
        "Most saved sessions scanned for memory material.",
        Kind::Int {
            min: 1,
            max: 10000,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.max_raw_memories",
        memory.local.max_raw_memories,
        LE,
        "Rollout memory",
        "Notes per consolidation",
        "Most extracted notes combined in one consolidation.",
        Kind::Int {
            min: 1,
            max: 2000,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.stage1_concurrency",
        memory.local.stage1_concurrency,
        LE,
        "Rollout memory",
        "Extractions at once",
        "Most extraction jobs that can run at the same time across Dial processes.",
        Kind::Int {
            min: 1,
            max: 32,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.stage1_lease_secs",
        memory.local.stage1_lease_secs,
        LE,
        "Rollout memory",
        "Extraction claim time (s)",
        "How long an extraction job holds its claim. Keep this longer than the model timeout.",
        Kind::Int {
            min: 1,
            max: 86400,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.stage1_retry_delay_secs",
        memory.local.stage1_retry_delay_secs,
        LE,
        "Rollout memory",
        "Extraction retry delay (s)",
        "Wait this long before retrying a failed extraction. A failure never saves an empty memory.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.consolidation_lease_secs",
        memory.local.consolidation_lease_secs,
        LE,
        "Rollout memory",
        "Consolidation claim time (s)",
        "How long a consolidation holds its claim. Keep this longer than the model timeout.",
        Kind::Int {
            min: 2,
            max: 86400,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.consolidation_retry_delay_secs",
        memory.local.consolidation_retry_delay_secs,
        LE,
        "Rollout memory",
        "Consolidation retry delay (s)",
        "Wait this long before retrying a failed consolidation. Pending notes are kept.",
        Kind::Int {
            min: 0,
            max: 86400,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.consolidation_heartbeat_secs",
        memory.local.consolidation_heartbeat_secs,
        LE,
        "Rollout memory",
        "Consolidation check-in (s)",
        "How often a running consolidation renews its claim. Must be shorter than the claim time.",
        Kind::Int {
            min: 1,
            max: 86399,
            step: 1
        },
        false
    ),
    field!(
        "memory.local.rollout_payload_percent",
        memory.local.rollout_payload_percent,
        LE,
        "Rollout memory",
        "Session text share",
        "Share of the context window that session text may use during extraction.",
        Kind::Float {
            min: 0.05,
            max: 0.9,
            step: 0.05
        },
        false
    ),
    field!(
        "memory.local.phase1_input_tokens",
        memory.local.phase1_input_tokens,
        LE,
        "Rollout memory",
        "Extraction text size (tokens)",
        "Approximate size cap for session text sent to extraction.",
        Kind::Int {
            min: 256,
            max: 1000000,
            step: 256
        },
        false
    ),
    field!(
        "memory.local.fallback_token_limit",
        memory.local.fallback_token_limit,
        LE,
        "Rollout memory",
        "Fallback text size (tokens)",
        "Size cap for session text when its real size is unknown. The model's context window still applies.",
        Kind::Int {
            min: 1024,
            max: 1000000,
            step: 256
        },
        false
    ),
    field!(
        "memory.local.summary_injection_tokens",
        memory.local.summary_injection_tokens,
        LE,
        "Rollout memory",
        "Summary prompt size (tokens)",
        "Largest amount of consolidated memory and lessons added to the prompt.",
        Kind::Int {
            min: 256,
            max: 1000000,
            step: 256
        },
        false
    ),
    field!(
        "memory.local.extraction_role",
        memory.local.extraction_role,
        LE,
        "Rollout memory",
        "Extraction model role",
        "Which model role runs extraction. Calls go through Dial's own model setup and usage tracking.",
        Kind::Enum(MODEL_ROLES),
        false
    ),
    field!(
        "memory.local.consolidation_role",
        memory.local.consolidation_role,
        LE,
        "Rollout memory",
        "Consolidation model role",
        "Which model role runs consolidation.",
        Kind::Enum(MODEL_ROLES),
        false
    ),
    field!(
        "memory.local.timeout_secs",
        memory.local.timeout_secs,
        LE,
        "Rollout memory",
        "Model timeout (s)",
        "Longest each memory model call may take. Calls that time out or are cancelled save nothing.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.endpoint",
        memory.hindsight.endpoint,
        LE,
        "Hindsight memory",
        "Service URL",
        "Address of the memory service. Use https://, or http:// for a server on this machine. Don't put passwords in the URL.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.api_key_env",
        memory.hindsight.api_key_env,
        LE,
        "Hindsight memory",
        "API key variable",
        "Name of an environment variable that holds the key, not the key itself. Required when the service is not on this machine.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.bank",
        memory.hindsight.bank,
        LE,
        "Hindsight memory",
        "Memory bank",
        "Name of the bank that stores memories. Each project gets its own bank unless scope is set to global.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.scoping",
        memory.hindsight.scoping,
        LE,
        "Hindsight memory",
        "Bank scope",
        "Global shares one bank across projects. Project keeps each project separate; project-tagged recall also includes shared memories.",
        Kind::Enum(&[
            ("global", "Global"),
            ("per-project", "Per project"),
            ("per-project-tagged", "Project tagged")
        ]),
        false
    ),
    field!(
        "memory.hindsight.recall_budget",
        memory.hindsight.recall_budget,
        LE,
        "Hindsight memory",
        "Remote recall budget",
        "Budget the service may spend on each recall. Dial counts the usage the service reports.",
        Kind::Enum(&[("low", "Low"), ("mid", "Mid"), ("high", "High")]),
        false
    ),
    field!(
        "memory.hindsight.recall_max_tokens",
        memory.hindsight.recall_max_tokens,
        LE,
        "Hindsight memory",
        "Recall size (tokens)",
        "Largest amount of context the service returns for each recall.",
        Kind::Int {
            min: 256,
            max: 100000,
            step: 256
        },
        false
    ),
    field!(
        "memory.hindsight.request_timeout_secs",
        memory.hindsight.request_timeout_secs,
        LE,
        "Hindsight memory",
        "Service timeout (s)",
        "How long to wait for the service on each request before giving up.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.retain_every_turns",
        memory.hindsight.retain_every_turns,
        LE,
        "Hindsight memory",
        "Save every N turns",
        "Save the conversation to memory after this many turns.",
        Kind::Int {
            min: 1,
            max: 1000,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.auto_recall",
        memory.hindsight.auto_recall,
        LE,
        "Hindsight memory",
        "Recall on first turn",
        "Prepare recall before the first message, and use it only if that message is actually sent.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.hindsight.retain_mission",
        memory.hindsight.retain_mission,
        LE,
        "Hindsight memory",
        "Retain focus",
        "Service-side instructions for what to keep. Set by the service operator, not the model.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.reflect_mission",
        memory.hindsight.reflect_mission,
        LE,
        "Hindsight memory",
        "Reflection focus",
        "Service-side instructions for how memories are summarized. Set by the service operator.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.auto_retain",
        memory.hindsight.auto_retain,
        LE,
        "Hindsight memory",
        "Save conversations automatically",
        "Save each main conversation's transcript. Saving on demand still works when this is off.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.hindsight.retain_mode",
        memory.hindsight.retain_mode,
        LE,
        "Hindsight memory",
        "Transcript save mode",
        "Full session replaces one stored document per session. Last turn saves new chunks as the conversation grows.",
        Kind::Enum(&[
            ("full-session", "Full session"),
            ("last-turn", "Incremental last turns")
        ]),
        false
    ),
    field!(
        "memory.hindsight.retain_overlap_turns",
        memory.hindsight.retain_overlap_turns,
        LE,
        "Hindsight memory",
        "Chunk overlap (turns)",
        "How many earlier turns each new chunk repeats, so context carries over.",
        Kind::Int {
            min: 0,
            max: 1000,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.retain_context",
        memory.hindsight.retain_context,
        LE,
        "Hindsight memory",
        "Transcript context",
        "Extra context from the operator sent with each saved transcript.",
        Kind::Text,
        false
    ),
    field!(
        "memory.hindsight.recall_types",
        memory.hindsight.recall_types,
        LE,
        "Hindsight memory",
        "Memory types to recall",
        "Which kinds of memory to recall. Defaults to world and experience.",
        Kind::StringList,
        false
    ),
    field!(
        "memory.hindsight.recall_context_turns",
        memory.hindsight.recall_context_turns,
        LE,
        "Hindsight memory",
        "Turns of context for recall",
        "How many recent turns of conversation to include in each recall. One means the latest message only.",
        Kind::Int {
            min: 1,
            max: 32,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.recall_max_query_chars",
        memory.hindsight.recall_max_query_chars,
        LE,
        "Hindsight memory",
        "Recall query length (chars)",
        "Longest recall query. The latest message is kept; the oldest context is dropped first.",
        Kind::Int {
            min: 256,
            max: 65536,
            step: 100
        },
        false
    ),
    field!(
        "memory.hindsight.reflect_timeout_secs",
        memory.hindsight.reflect_timeout_secs,
        LE,
        "Hindsight memory",
        "Reflection timeout (s)",
        "How long a reflection request may run before it stops.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.recall_timeout_secs",
        memory.hindsight.recall_timeout_secs,
        LE,
        "Hindsight memory",
        "Recall timeout (s)",
        "How long a recall may take. Recalls during compaction or first-turn preparation stop after five seconds.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.retain_timeout_secs",
        memory.hindsight.retain_timeout_secs,
        LE,
        "Hindsight memory",
        "Save timeout (s)",
        "How long a save request may take before it stops.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.hindsight.mental_models_enabled",
        memory.hindsight.mental_models_enabled,
        LE,
        "Hindsight memory",
        "Use mental models",
        "Use the service's saved summaries as extra context, filtered to this project. Treated as untrusted text.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.hindsight.mental_model_auto_seed",
        memory.hindsight.mental_model_auto_seed,
        LE,
        "Hindsight memory",
        "Create missing starter models",
        "Create starter summaries for preferences, conventions and decisions when they are missing. Existing ones are never overwritten.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.hindsight.mental_model_max_render_chars",
        memory.hindsight.mental_model_max_render_chars,
        LE,
        "Hindsight memory",
        "Mental model size (chars)",
        "Largest size of mental-model text added to the prompt.",
        Kind::Int {
            min: 256,
            max: 65536,
            step: 256
        },
        false
    ),
    field!(
        "memory.sharpshooter.model",
        memory.sharpshooter.model,
        LE,
        "Decision memory",
        "Model override",
        "Optional model for decision notes, as `provider/model` or a name on the memory role's provider. Leave empty to use the model role below (memory by default); set, it replaces that role.",
        Kind::Text,
        false
    ),
    field!(
        "memory.sharpshooter.model_role",
        memory.sharpshooter.model_role,
        LE,
        "Decision memory",
        "Memory model role",
        "Which model role reads sessions for decisions.",
        Kind::Enum(MODEL_ROLES),
        false
    ),
    field!(
        "memory.sharpshooter.interval_minutes",
        memory.sharpshooter.interval_minutes,
        LE,
        "Decision memory",
        "Consolidate every (minutes)",
        "How often decisions are consolidated for each project.",
        Kind::Int {
            min: 1,
            max: 1440,
            step: 1
        },
        false
    ),
    field!(
        "memory.sharpshooter.injection_token_limit",
        memory.sharpshooter.injection_token_limit,
        LE,
        "Decision memory",
        "Decision prompt size (tokens)",
        "Largest amount of decision notes added to the prompt. They are treated as untrusted text.",
        Kind::Int {
            min: 256,
            max: 1000000,
            step: 256
        },
        false
    ),
    field!(
        "memory.sharpshooter.max_file_lines",
        memory.sharpshooter.max_file_lines,
        LE,
        "Decision memory",
        "Lines per decision file",
        "Longest decision file. The hard limit is 120 lines; you can set a smaller one.",
        Kind::Int {
            min: 1,
            max: 120,
            step: 1
        },
        false
    ),
    field!(
        "memory.sharpshooter.max_parallel",
        memory.sharpshooter.max_parallel,
        LE,
        "Decision memory",
        "Decision extractions at once",
        "Most decision extractions that can run at the same time across workers.",
        Kind::Int {
            min: 1,
            max: 32,
            step: 1
        },
        false
    ),
    field!(
        "memory.sharpshooter.lease_secs",
        memory.sharpshooter.lease_secs,
        LE,
        "Decision memory",
        "Decision claim time (s)",
        "How long a decision job holds its claim. Keep this longer than the model timeout.",
        Kind::Int {
            min: 1,
            max: 86400,
            step: 1
        },
        false
    ),
    field!(
        "memory.sharpshooter.timeout_secs",
        memory.sharpshooter.timeout_secs,
        LE,
        "Decision memory",
        "Decision model timeout (s)",
        "Longest a decision extraction or consolidation may take.",
        Kind::Int {
            min: 1,
            max: 3600,
            step: 1
        },
        false
    ),
    field!(
        "memory.semantic.embedding",
        memory.semantic.embedding,
        LE,
        "Semantic memory",
        "Embeddings",
        "Off keeps recall keyword-only. Automatic uses embeddings when a route is configured: the URL and model override below when both are set (the override wins), otherwise the embedding role (Alt+M). On requires one of them. A half-filled override is rejected.",
        Kind::Enum(&[("off", "Off"), ("auto", "Automatic"), ("on", "On")]),
        false
    ),
    field!(
        "memory.semantic.embedding_base_url",
        memory.semantic.embedding_base_url,
        LE,
        "Semantic memory",
        "Embedding service URL",
        "Advanced override, set together with the model override: an OpenAI-compatible base address, for example https://api.example.com/v1. When both are set they win over the embedding role. Plain http:// works only on this machine.",
        Kind::Text,
        false
    ),
    field!(
        "memory.semantic.embedding_model",
        memory.semantic.embedding_model,
        LE,
        "Semantic memory",
        "Embedding model",
        "Enter picks the embedding role (Alt+M): discovered embedding models, or a curated Ollama model to set up (Ctrl+L). A typed name here is an advanced OpenAI-compatible override that needs the Embedding service URL too.",
        Kind::Choice(crate::config::embeddings::CHOICES),
        false
    ),
    field!(
        "memory.semantic.embedding_api_key_env",
        memory.semantic.embedding_api_key_env,
        LE,
        "Semantic memory",
        "API key variable",
        "Name of the environment variable that holds the embedding service key, for the URL override. Leave empty for a local server without authentication.",
        Kind::Text,
        false
    ),
    field!(
        "memory.semantic.embedding_timeout_secs",
        memory.semantic.embedding_timeout_secs,
        LE,
        "Semantic memory",
        "Embedding timeout (s)",
        "Longest a single embedding request may take before recall falls back to keywords.",
        Kind::Int {
            min: 1,
            max: 120,
            step: 1
        },
        false
    ),
    field!(
        "memory.semantic.recall_limit",
        memory.semantic.recall_limit,
        LE,
        "Semantic memory",
        "Facts recalled per query",
        "Most facts added to a prompt for one query.",
        Kind::Int {
            min: 1,
            max: 50,
            step: 1
        },
        false
    ),
    field!(
        "memory.semantic.token_budget",
        memory.semantic.token_budget,
        LE,
        "Semantic memory",
        "Recall budget (tokens)",
        "Largest amount of recalled text added to a prompt. Recalled facts are treated as untrusted text.",
        Kind::Int {
            min: 64,
            max: 16000,
            step: 64
        },
        false
    ),
    field!(
        "memory.semantic.auto_retain",
        memory.semantic.auto_retain,
        LE,
        "Semantic memory",
        "Retain facts automatically",
        "After substantive turns, extract durable facts with the memory role (Alt+M) and store them in the background. Pauses while the memory role is unassigned; it never falls back to the main model. Each turn is extracted once. Turn text is redacted and treated as untrusted.",
        Kind::Bool,
        false
    ),
    field!(
        "memory.semantic.consolidate_interval_minutes",
        memory.semantic.consolidate_interval_minutes,
        LE,
        "Semantic memory",
        "Duplicate merge interval (min)",
        "How often near-duplicate facts are merged, keeping the older one. 0 turns merging off.",
        Kind::Int {
            min: 0,
            max: 10080,
            step: 5
        },
        false
    ),
];
