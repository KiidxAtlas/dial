# Configuration

Configuration layering, unlimited execution defaults and an annotated example configuration. Feature-specific settings are documented alongside each feature; see the [README index](../README.md#documentation).

## Configuration layers

Layered TOML, user then project-specific: `~/.dial/config.toml`, `~/.dial/projects/<name>-<hash>/config.toml`, then flags. Never read from the project directory.

Loading uses the same bounded private-file reader as settings updates. Missing layers are allowed;
unreadable, invalid UTF-8, oversized, linked or non-regular layers produce a path-specific error rather
than silently falling back to different settings. Files must be owned by the current user without
hard links; symlinked files and user-owned symlink ancestors are refused. Valid files are owner-private.

Learning stores and adapters are additionally isolated under
`~/.dial/projects/<name>-<hash>/models/<identity>/`, so switching providers/models does not reuse an
incompatible adapter. Sessions, skills, memory and prompt history remain project-scoped.

## Unlimited execution defaults

Most configurable execution limits and result/context size caps default to `0`, meaning **unlimited**:
worker idle timeout, `harness.max_steps`, `harness.max_tokens`, reasoning guard limits,
solve/escalation budgets, background daily compute, recalled context, history retention, and benchmark
case deadlines. A positive value restores a cap. Other deadlines and previews have finite defaults:
`shell.bash_timeout_secs` 300, hook `timeout_secs` 30, MCP `timeout_secs` 120 and `connect_timeout_secs`
30, extension `timeout_secs` 30, `request_recovery.silence_secs` 120, `efficiency.ambient_context_bytes`
8192, `efficiency.lightweight_requests` 4 and `efficiency.cache_pressure_percent` 75. The exception is
`verification.repair_attempts`: `0` selects the default of 3 automatic repair rounds (when
`verification.automatic_checks` is on), because a check that can never pass would otherwise re-prompt
the model forever.
Existing explicit settings are preserved; changing defaults does not rewrite saved overrides.

`harness.max_tokens = 0` means no operator output cap, not a literal zero-token API request.
Dial uses provider-advertised output capacity when available, otherwise the selected context capacity;
model profiles and the provider's actual output/context limits still apply. Context-window settings
remain `auto`, rather than pretending a model has infinite context. Sampling/training parameters,
approvals, sandboxing, host restrictions, protocol frame protections and loop-safety rules are unchanged.
Unlimited calls can wait indefinitely or consume substantial memory and money; Esc/Ctrl-C still cancels
interactive work. Configure positive limits where that is not acceptable.

Set `efficiency.enabled=false` to disable these shared economy policies. Each completed/failed task
stores an `efficiency` completion snapshot in session control state. Structured print output and RPC
emit an `efficiency` event and include it in their final result/state. The report separates main and
worker usage by protocol/model, records request/retry and usage-coverage counts, and includes main-loop
schema/system/history/request-tail and tool-result byte measurements. These bytes are projections,
not billed tokens; missing provider counts stay distinguishable through coverage fields. Cache-token
semantics differ by provider, and late workers can update the live usage ledger after the completion
snapshot. Compare like-for-like tasks and correctness first; agent completion alone is not independent
proof of correctness or subscription quota savings.

## Example configuration

```toml
remote_model_idle_timeout_secs = 300         # 0 disables; local model servers are never limited

[reasoning_guard]          # operator-owned; not mutated by harness improvement
enabled = true
max_pre_action_secs = 0
max_reasoning_bytes = 0
require_plan = true         # after a gate hit with no open todo, only `todo` is offered until a plan exists

[codebase]
enabled = true
context_max_bytes = 0

[ui]
theme = "dark"                    # dark | light | mono | gruvbox | nord | paper | auto
status = "default"                 # default | minimal | compact | full | nerd | ascii | custom
status_left = ["spinner", "model", "path", "git"]
status_right = ["context"]
status_separator = "powerline-thin"
composer_shape = "box"             # box | line
symbols = "auto"                   # auto | unicode | nerd | ascii
show_tools = true
history_limit = 0

[model_roles.small]
provider = "mlx"
model = "mlx-community/Qwen2.5-0.5B-Instruct-4bit"

[model_roles.tester]
provider = "ollama"
model = "your-model-id"
base_url = "http://127.0.0.1:11434/v1"
```

The model picker persists `provider = "mlx"` when selecting local inference, explicitly clearing an
inherited remote endpoint. Helper roles use the same rule. A project role with `provider = "inherit"`
follows the main model even if a global role assignment exists. An admitted harness may pin an edit mode;
the settings panel labels that mode as pinned instead of claiming an ineffective override.

```toml
model = "mlx-community/Qwen2.5-7B-Instruct-4bit"
approval = "write"        # ask | write | auto | readonly
integrations = ["bg", "web_fetch", "web_search", "lsp", "ast_search", "ast_edit", "dap"]
context_window = "auto" # defaults to 32768 tokens; serving endpoints may cap to loaded context
[sandbox]
mode = "dev"              # dev | strict | off
allow_network = true
extra_write = ["~/.cargo", "~/.npm"]
[endpoint]                # optional: OpenAI-compatible server instead of local MLX
base_url = "http://127.0.0.1:1234/v1"
api_key_env = "LMSTUDIO_KEY"
[harness]                 # the part the improvement loop is allowed to change
system_prompt = "Work in small verified steps."
tools = ["read","edit","write","glob","grep","bash","todo","tool_search","tool_load","artifact_read","working_state"]
[tools.lint]              # operator tools: trusted programs, not sandboxed
program = "/usr/local/bin/mylint"
timeout_secs = 0
[subagents]               # custom scouts: extra instructions, selected with task agent = "security"
security = "Focus on injection, authentication and secret-handling bugs."
[web]
search_url = "https://search.example/api?q={query}"   # optional; default is DuckDuckGo HTML
search_api_key_env = "SEARCH_KEY"                    # optional bearer token variable
[lsp.rust]                                           # override or add a language server
program = "/opt/homebrew/bin/rust-analyzer"
extensions = ["rs"]
[mcp.docs]                                           # stdio MCP server (trusted program)
program = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
timeout_secs = 0

[dap.python]                 # approved local DAP adapter process
program = "/workspace/bin/python-debug-adapter"
args = []
timeout_secs = 0

[verification]
enabled = true
repair_attempts = 0

[[verification.commands]]
name = "focused-tests"
command = "cargo test --offline --lib"
file_extensions = ["rs"]
timeout_secs = 0

[improve]
enabled = true
background = true
interval_secs = 21600
min_new_turns = 5
selfplay_tasks = 24
gate_primary_cases = 30
gate_guard_cases = 20
max_candidates = 0
alpha = 0.1
min_net_wins = 2
lora_min_examples = 24
teach = false              # "Teach this model": record local-model coding sessions and serve admitted versions
learn_from_sessions = true
monitor_min_turns = 8
monitor_drop = 15.0
drift_tolerance = 3.0

[training]
enabled = true
interval_secs = 21600
iters = 32
batch_size = 1
rank = 8
num_layers = 4
max_seq_length = 8192
```
