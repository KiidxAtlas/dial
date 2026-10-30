//! Complete management routes for structured configuration and less common operator settings.
//! These edit one real config value through the same validated settings transaction as the
//! simple controls. Parent-table routes deliberately include optional fields and dynamic keys;
//! adding a field to such a table does not require a second configuration pipeline.
use super::*;

macro_rules! structured {
    ($path:literal, $tab:expr, $section:literal, $label:literal, $help:literal) => {
        def!($path, $tab, $section, $label,
            concat!($help, " Enter a TOML inline table or array. The editor contains only the selected scope's stored value, not inherited values. Save replaces that scope's value; Delete removes its override. The complete configuration is validated before saving."),
            Kind::Toml)
    };
}

pub(super) static DECLARATIONS: &[SettingDef] = &[
    def!(
        "python",
        MO,
        "Runtime",
        "MLX Python",
        "Python executable for local MLX inference.",
        Kind::Text,
        true
    ),
    def!(
        "worker",
        MO,
        "Runtime",
        "MLX worker",
        "Local inference worker script path.",
        Kind::Text,
        true
    ),
    def!(
        "use_case",
        MO,
        "Runtime",
        "Use case",
        "Project use-case label; blank uses the project directory name.",
        Kind::Text
    ),
    def!(
        "model",
        MO,
        "Configuration",
        "Model identity",
        "Raw model name. The Models panel offers model discovery and role assignment.",
        Kind::Text
    ),
    def!(
        "provider",
        MO,
        "Configuration",
        "Provider identity",
        "Optional provider override; Delete inherits the provider. Use the Providers panel for setup.",
        Kind::Text
    ),
    def!(
        "model_profile",
        MO,
        "Configuration",
        "Selected model profile",
        "Optional named exact-model profile; Delete returns to automatic selection.",
        Kind::Text
    ),
    def!(
        "snowflake_account",
        MO,
        "Configuration",
        "Snowflake account",
        "Account identifier substituted into the Snowflake endpoint.",
        Kind::Text
    ),
    def!(
        "cloudflare_account_id",
        MO,
        "Configuration",
        "Cloudflare account",
        "Cloudflare account identifier for AI Gateway.",
        Kind::Text
    ),
    def!(
        "cloudflare_gateway_id",
        MO,
        "Configuration",
        "Cloudflare gateway",
        "Cloudflare AI Gateway identifier.",
        Kind::Text
    ),
    def!(
        "codex_websocket",
        MO,
        "Requests",
        "Codex WebSocket",
        "Prefer Responses WebSocket transport; handshake failure falls back to SSE.",
        Kind::Bool
    ),
    structured!(
        "endpoint",
        MO,
        "Configuration",
        "Endpoint configuration",
        "Optional endpoint: url, model, api_key_env, allow_insecure, protocol, headers and account_id. Credential-bearing contents are masked."
    ),
    structured!(
        "providers",
        MO,
        "Configuration",
        "Provider definitions",
        "Named providers, including auth (auto, api-key or oauth), protocol, request headers, account pin and context settings. Authentication selection does not delete credentials. Contents are masked; reference secret environment variables instead of literal tokens."
    ),
    structured!(
        "model_roles",
        MO,
        "Configuration",
        "Role assignments",
        "Role names map to { provider = \"name\", model = \"identity\" }. Also managed in Models."
    ),
    structured!(
        "model_favorites",
        MO,
        "Configuration",
        "Favorite models",
        "Array of { provider = \"name\", model = \"identity\" }. Also managed in Models."
    ),
    structured!(
        "model_profiles",
        MO,
        "Configuration",
        "Model profiles",
        "Exact-model capabilities, sampling and reasoning profiles keyed by profile name."
    ),
    structured!(
        "model_presets",
        MO,
        "Configuration",
        "Model presets",
        "Named role/model assignments and thinking levels. Also managed by /modelpreset."
    ),
    structured!(
        "local_runtime",
        MO,
        "Runtime",
        "Local runtime options",
        "Optional draft_model, num_draft_tokens, kv_bits, kv_group_size and quantized_kv_start."
    ),
    structured!(
        "harness",
        MO,
        "Configuration",
        "Harness configuration",
        "System prompt, memory, tools, subagents, step/token limits, temperature and optional edit_mode."
    ),
    structured!(
        "request_recovery",
        MO,
        "Requests",
        "Request recovery",
        "Silence recovery timing and retry_once."
    ),
    structured!(
        "efficiency",
        MO,
        "Configuration",
        "Efficiency configuration",
        "Context budgets, lightweight request count and cache pressure."
    ),
    structured!(
        "compaction",
        MO,
        "Configuration",
        "Complete compaction configuration",
        "All compaction options, including preserve_prefix and per-model maps."
    ),
    structured!(
        "gate",
        "Improvement",
        "Configuration",
        "Evaluation gate",
        "Optional suites array (name, path, primary), max_regression and min_improvement."
    ),
    structured!(
        "training",
        "Improvement",
        "Configuration",
        "Training configuration",
        "All training options including train_data and valid_data paths."
    ),
    structured!(
        "improve",
        "Improvement",
        "Configuration",
        "Improvement configuration",
        "All improvement options including idle_minutes, require_ac_power, daily_compute_minutes and max_daily_usd."
    ),
    structured!(
        "laya",
        "Improvement",
        "Configuration",
        "Laya configuration",
        "All learning-engine options, including engine selection."
    ),
    structured!(
        "shell",
        "Tools",
        "Configuration",
        "Shell configuration",
        "Timeouts, auto-background timing, PTY, backend, output minimization and git guard."
    ),
    structured!(
        "bash_interceptor",
        "Tools",
        "Configuration",
        "Bash interceptor",
        "Enable or disable dedicated-tool hints; mode is hint or block."
    ),
    structured!(
        "browser",
        "Tools",
        "Configuration",
        "Browser configuration",
        "Node/Playwright paths, allowed_hosts, headless, stealth, recording and timeout options."
    ),
    structured!(
        "kernel",
        "Tools",
        "Configuration",
        "Kernel configuration",
        "Python executable, venv selection and timeout."
    ),
    structured!(
        "ssh",
        "Tools",
        "Configuration",
        "SSH policy",
        "Host-key checking, connection timeout and control connection persistence."
    ),
    structured!(
        "ssh_hosts",
        "Tools",
        "Configuration",
        "SSH host definitions",
        "Named host, user, port and key path; never private key contents. Also managed by /ssh."
    ),
    structured!(
        "tools",
        "Tools",
        "Configuration",
        "Custom tool definitions",
        "Named program, args, timeout_secs, description and optional parameters JSON Schema. Contents are masked."
    ),
    structured!(
        "subagents",
        "Tools",
        "Configuration",
        "Subagent prompts",
        "Named subagent instruction strings."
    ),
    structured!(
        "agents",
        "Tools",
        "Configuration",
        "Agent definitions",
        "Named agent types, instructions, tool sets, models, budgets and spawn policies."
    ),
    structured!(
        "task",
        "Tools",
        "Configuration",
        "Task configuration",
        "All worker concurrency, recursion, persistence, discovery, isolation and provider_concurrency options."
    ),
    structured!(
        "hooks",
        "Tools",
        "Configuration",
        "Lifecycle hooks",
        "Array of hook definitions. Layers append hooks rather than replacing inherited hooks. Contents are masked."
    ),
    structured!(
        "mcp",
        "Tools",
        "Configuration",
        "MCP server definitions",
        "Named stdio or HTTP servers, programs, arguments, env, OAuth and lifecycle options. Contents are masked; credentials stay private."
    ),
    structured!(
        "mcp_settings",
        "Tools",
        "Configuration",
        "MCP reserved switches",
        "discover and project_approval are reserved: accepted in config but currently have no runtime effect."
    ),
    structured!(
        "lsp",
        "Tools",
        "Configuration",
        "Language server configuration",
        "Write-through formatting/diagnostics/start switches plus named language server overrides."
    ),
    structured!(
        "dap",
        "Tools",
        "Configuration",
        "Debug adapters",
        "Named Debug Adapter Protocol executable and argument definitions."
    ),
    structured!(
        "extensions",
        "Tools",
        "Configuration",
        "Extension configuration",
        "Extension discovery, timeout, plugin definitions and flags. Contents are masked."
    ),
    structured!(
        "verification",
        "Tools",
        "Configuration",
        "Verification configuration",
        "Executable checks, requirements, runners, inputs, review and repair policies."
    ),
    structured!(
        "solve",
        "Tools",
        "Configuration",
        "Isolated solve configuration",
        "Enable bounded patch search and set candidate, concurrency (default 1), step and snapshot limits."
    ),
    structured!(
        "escalation",
        "Tools",
        "Configuration",
        "Escalation configuration",
        "Stronger-model advice policy, including evidence byte limits."
    ),
    structured!(
        "git",
        "Tools",
        "Configuration",
        "Git policy",
        "Hook and signing policy including bypass_hooks."
    ),
    structured!(
        "web",
        "Tools",
        "Configuration",
        "Web configuration",
        "Search URL/provider/API-key environment reference and reader chain."
    ),
    def!(
        "integrations",
        "Tools",
        "Configuration",
        "Integration tools",
        "Comma-separated integration tool names offered to the model.",
        Kind::StringList
    ),
    def!(
        "foreign_sources",
        "Tools",
        "Configuration",
        "Foreign context sources",
        "Opt-in tool names whose user rules/context are loaded, for example codex or gemini.",
        Kind::StringList
    ),
    structured!(
        "budget",
        MO,
        "Requests",
        "Cost budgets",
        "Maximum turn and session cost in USD (hard caps; zero disables each), plus warn_session_cost_usd and warn_turn_cost_usd notices shown once per crossing without stopping work."
    ),
    structured!(
        "image.api_key_env",
        TO,
        "Image generation",
        "API key variable",
        "Name of an environment variable that holds the image endpoint's key, not the key itself. Unset uses the endpoint's standard variable or a key saved through /providers."
    ),
    structured!(
        "image.models",
        TO,
        "Image generation",
        "Per-model image settings",
        "Generation settings per image model, keyed by any bare name and matched by each entry's exact model: size, steps, seed, loras (path and scale), quality, background, moderation. /image settings edits the current image model's entry."
    ),
    structured!(
        "retry.fallback_chains",
        MO,
        "Requests",
        "Fallback chains",
        "Provider/model identities map to ordered fallback model lists."
    ),
    structured!(
        "stats",
        "Experimental",
        "Configuration",
        "Stats server",
        "Enable the local statistics server and choose its port."
    ),
    structured!(
        "telemetry",
        "Experimental",
        "Configuration",
        "Telemetry configuration",
        "Collector endpoint, enabled signals, service name, buffering and timing. Set credential headers in the separate masked field, not here."
    ),
    structured!(
        "telemetry.headers",
        "Experimental",
        "Configuration",
        "Telemetry headers",
        "Collector headers, possibly including credentials. Contents are masked."
    ),
    structured!(
        "collab",
        "Experimental",
        "Configuration",
        "Collaboration configuration",
        "Relay URL, web URL and guest steering policy."
    ),
    structured!(
        "share",
        "Experimental",
        "Configuration",
        "Sharing configuration",
        "Sharing server URL and policy."
    ),
    def!(
        "secrets.values",
        "Tools",
        "Configuration",
        "Masked literal secrets",
        "Comma-separated additional secrets to mask in provider messages. Values are hidden while viewing and editing; each must be at least eight bytes. Delete removes the override.",
        Kind::StringList
    ),
    structured!(
        "voice.openai",
        VO,
        "Configuration",
        "OpenAI audio configuration",
        "Audio API base URL, API-key environment reference and timeout."
    ),
    structured!(
        "voice.tts",
        VO,
        "Configuration",
        "Speech output configuration",
        "Backend, voice, speed, player, local program/model paths and auto-speak limits."
    ),
    structured!(
        "voice.stt",
        VO,
        "Configuration",
        "Speech input configuration",
        "Capture backend/command, language, silence timings, transcription models/programs, timeout and submit trigger."
    ),
    structured!(
        "voice.live",
        VO,
        "Configuration",
        "Live voice configuration",
        "Live voice model, voice and instructions."
    ),
];
