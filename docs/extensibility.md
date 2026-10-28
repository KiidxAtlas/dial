# Extensions and project context

Dial has two extension boundaries. Trusted operator integrations can register tools and commands through the Rust `Extension` SDK or through configured stdio extension programs. Workspace-provided guidance and skills are untrusted input: they are read-only, size-limited, and never execute programs. Dial never reads configuration from the workspace.

## Lifecycle hooks

Configure hooks with `[[hooks]]`; `event` names a lifecycle point, `command` is an argv array (never shell text), and `matcher` optionally selects tool calls by anchored regular expression. Hooks receive a JSON event on stdin. A JSON response can rewrite `input` before a tool call or `content` after a tool result, and can return a stop/block verdict where supported. Hook arrays append across configuration layers, preserving global hooks when project settings are loaded. The default deadline is 30 seconds. `fail_closed` defaults to true for `pre_tool_use` and false for observational events; set it explicitly when policy requires otherwise.

Supported lifecycle points include user prompt submission, session start/end, before compaction, before subagent spawn, before provider requests, pre/post tool use, and stop. Matchers are compiled and cached. A failing fail-closed policy hook prevents the action; hook failures must not be silently converted into permission.

## Custom tools

`[tools.<name>]` declares `program`, optional `args` and `timeout_secs`, plus `description` and a JSON Schema `parameters` object. When parameters are supplied, the tool receives the full JSON arguments object on stdin; otherwise it receives a single `input` string. Custom programs are operator-trusted and run outside the workspace sandbox. Do not configure workspace-controlled executables.

## Runtime extensions

Runtime plugins are operator-installed stdio programs: those declared under `[extensions.plugins.<name>]` and those installed with `dial plugin install` (started from the user `plugins` directory). Dial does not scan `~/.dial/extensions` or the project for plugin executables, so no project-local extension runs without an explicit configuration entry. Plugins communicate using JSON-RPC messages over stdio and may register tools, commands, shortcuts and flags, update active tools, and observe lifecycle events. They run with the privileges of the Dial process; only install code you trust. The Rust SDK is for embedding trusted native integrations and does not bypass tool approvals or workspace controls. Plugins and skills offered by marketplaces are found and installed in `/extensions`; see Marketplaces below.

### Commands and declarative panels

A stdio plugin advertises slash-command names in its `initialize` result, for example
`{"commands":["demo"]}`. Command-only plugins need no event subscription. Names are nonempty ASCII
graphic strings of at most 48 bytes, without a leading `/`. Commands appear in action navigation;
built-ins keep precedence. Invocation sends `command` with `{"name":"demo","args":"..."}`.

The result may contain `{"text":"..."}` or a `panel`:

```json
{
  "panel": {
    "title": "Review choice",
    "generation": 7,
    "elements": [
      {"type": "input", "id": "name", "label": "Name", "value": ""},
      {"type": "select", "id": "mode", "label": "Mode", "options": ["fast", "safe"], "selected": 1},
      {"type": "button", "id": "save", "label": "Save", "primary": true}
    ]
  }
}
```

Elements are `text`, `markdown` (wrapped source), `list`, `input`, `select` and `button`. A panel
requires a title and a button. The layout budget is 64 widgets plus nested list rows/select choices;
each author-supplied string is limited to 2048 bytes. Interactive IDs must be unique, at most 64
ASCII letters, digits, `_` or `-`; selects require a valid starting option. Invalid layouts are
refused rather than partially displayed.

The UI owns focus, editing and cancellation. Closing sends `command.panel` with the command name,
the opened `generation`, and an `outcome`: `{"status":"submitted","button":"save","values":{"name":"Ada","mode":1}}`,
`{"status":"cancelled"}`, or `{"status":"unsupported","reason":"..."}` for a surface without panels.
Input values are strings; select values are zero-based indexes. The close reply may contain text.
Requests still use plugin timeout/cancellation and loaded-plugin checks; an unloaded plugin does
not receive a fabricated successful close. Embedders use the same `Panel`/`Outcome` contract
through `Ui::run_panel`. A panel outcome is human input, never tool approval or an expanded grant.

### Protocol 2: capabilities, actions and UI slots

The host's `initialize` request sends `protocolVersion: "2"`, `supportedProtocolVersions: ["1","2"]`,
and an empty `capabilities` object. A plugin that answers with `"protocolVersion": "2"` and a
`capabilities` array of names opts in. Plugins that omit the version, or answer `"1"`, stay legacy:
they keep hooks, commands and panels, and any `actions` or `ui` entries they return are refused with
a notice. An unsupported version fails startup. Unknown capability names are ignored.

Protocol 2 replies may contain an `actions` array. Each entry needs a `type` and the capability it
requires; an entry without its capability is refused with a notice while other entries still apply.

- `ui` with `slot` (`status`, `header`, `footer`, `title`) and `lines`: display-only text, sanitized
  (control sequences removed, newlines flattened) and clamped to 240 characters per line. Status and
  title take one line; header and footer take up to 8. An empty `lines` clears the slot. Capabilities:
  `ui.status`, `ui.header`, `ui.footer`, `ui.title`. Placement: header rows sit above the composer;
  status and footer rows sit below it; a title replaces the session title in the terminal title.
  Empty slots take no rows. When several plugins set a title, the first one wins.
- `send_user_message` `{text}` (`actions.send_user_message`), `append_entry` `{kind,text}`
  (`actions.append_entry`), `set_model` `{model,effort?}` (`actions.set_model`), `set_thinking`
  `{effort}` (`actions.set_thinking`), `register_provider` `{name,config}` and `unregister_provider`
  `{name}` (`actions.register_provider`, `actions.unregister_provider`). Names are ASCII letters,
  digits and `._-/:`, at most 64 characters. Messages are capped at 256 KiB and entries at 64 KiB.
  Replies carry at most 16 actions.

Actions are requests, not commands. They are applied at the idle boundary, never mid-turn. Before
applying one, Dial shows an approval prompt that names the plugin and the exact change. These require
approval (Allow once or Deny): `send_user_message`, `set_model`, `set_thinking`, `register_provider` and
`unregister_provider`. `append_entry` needs none. A declined action is reported as a notice and dropped.
Model and thinking changes are session-only. Provider changes are saved to the global settings like a
manual change. The provider prompt shows the endpoint and the name of the environment variable that
holds the key, never the key or header values. A key given as a literal value is refused, and the
refusal does not echo it.

`append_entry` may carry an optional `renderer`: `{title?, fields?: [{label,value}], markdown?}`, with at
most 12 fields and 4 KiB of markdown. Dial shows it as a transcript notice titled by `title` (default
`<plugin>: <kind>`): fields first, then the markdown text. Markdown is shown as plain text and is never
interpreted. Without a renderer the sanitized `text` is the body. Entries are recorded in the session
under the `plugin.entry` custom type, which model context never reads. When a session is resumed, each
entry is rebuilt into the transcript between the messages it followed, through the same renderer.

Editor-mode contributions are not supported. Composer keystrokes, including typed secrets, would have to
route to an external process, and protocol 2 has no declarative form that keeps them out.

Autocomplete: a plugin that declares `autocomplete` receives `autocomplete` with `{"query":"..."}`
and replies `{"items":[{"value","label?","description?"}]}`. At most 20 suggestions are shown, and
empty values are dropped. Suggestions appear in the slash-command menu as extension commands while the
command name is typed; built-in commands win on a name collision. A value with whitespace is kept and
inserted single-quoted, as one shell word. Lookups run on a worker, bounded by the plugin timeout, and
answers for a query the user has moved past are discarded, so a slow plugin never delays typing.

Observational events are delivered in order on a background worker to protocol 2 plugins that list
them in `events`. Each payload is a JSON object that carries `session_id`. Their replies may carry
actions and UI slots but cannot block or rewrite the agent. A full queue of 256 drops events with a
notice. Text fields are truncated to 8000 characters.

- `turn_start` `{session_id, prompt}`: before `user_prompt_submit` hooks run; `prompt` is the original message.
- `turn_end` `{session_id, outcome, steps, interrupted}`: once per turn, after the outcome is settled.
- `message_start` `{session_id, step}`: before each model request.
- `message_end` and `tool_call`/`tool_result` are not observer events: they are aliases of the blocking
  hooks `assistant_message`, `pre_tool_use` and `post_tool_use`. Subscribe to those hook names to see
  them; each is delivered once, through the blocking path, with its payload `{session_id, step, text, tool_calls}`
  (`message_end`) or `{session_id, call_id, name, arguments}` / `{session_id, call_id, name, is_error, output}`.
- `mode_changed` `{session_id, mode, enabled}`: when plan mode flips (`mode` is `"plan"`).
- `model_changed` `{session_id, provider, model}`: after a model switch, including on session resume.
- `mcp_notification` `{session_id, server, kind, added?, removed?, changed?, error?}`: an MCP change. `kind` is one of `tools_changed`, `tools_refreshed`, `tools_refresh_failed`, `prompts_changed`, `resources_changed`, or `resource_updated`. `added`, `removed`, and `changed` list tool names on `tools_refreshed`; `error` is set on `tools_refresh_failed`.

Hook-alias names listed in `events` are delivered by the blocking path only, so the observer queue
never sends them. Blocking hook events keep their synchronous `rewrite` contract.

`user_prompt_submit` accepts a reply `{"prompt":"..."}` that replaces the user message for later hooks
and the model request. The replacement must be non-empty and at most 256 KiB, or the turn fails.
Approval events fire only from the turn's own UI reducer, so they cover the parent agent's tool calls
(child workers are not observed).
- `message_update` `{session_id, text}`: the accumulated answer text of the current assistant message.
  Sent at most ten times a second, with the final text delivered before `message_end`. Text from a
  discarded attempt is never sent. Subscribe only when you need streaming; other observers pay nothing.
- `tool_approval_requested` `{session_id, call_id, name}`: a user prompt for a tool call is about to show.
- `tool_approval_resolved` `{session_id, call_id, name, decision, source}`: the call was decided. `source`
  is `user` after a prompt, or `rule` when a permission rule decided it without a prompt (no matching
  requested event). `decision` is `allow`, `allow_always`, `allow_project` or `deny`; rule decisions are
  `allow` or `deny`.

## Context and skills

Instruction files such as `AGENTS.md` and `CLAUDE.md` are discovered from the project hierarchy. Instruction files may import others with `@path` (for example `@docs/style.md`): imports expand in place inside an `<import file="…">` block naming their source, at most 5 levels deep, 32 imports per file, and 32 KiB per file. Targets must stay inside the project tree or the home directory; cycles, escapes (including symlinks), and `@` tokens inside fenced or inline code are left as written or refused with a visible `[@path not imported: …]` marker. `@path` references in a user message are expanded into read-only, size-limited content confined to the project root (absolute, `..` and escaping symlinks are refused). Dial also discovers supported context, command, and skill files from common coding-agent directories (including `.claude`, `.codex`, `.gemini`, `.cursor`, `.windsurf`, `.cline`, and `.github`). These files are model context, not executable configuration.

## Skill packages

`dial skills install` (and `/skills install`) fetches skills from a Git repository into Dial's
content-addressed store. Source syntax is `url[@ref][#path]`:

- `url`: any URL the system `git` accepts, including `file://` for local repositories. `git` runs
  with prompts disabled, a 300-second timeout, and `init --template=` so user git hooks and templates
  never load.
- `@ref`: a branch, tag, or commit. Omitted means the remote's default branch.
- `#path`: a relative directory inside the repository. Omitted means the whole repository. `..`,
  absolute paths and drive prefixes are refused.

A package is every directory containing `SKILL.md` under the selected path. Installed skill names
come from frontmatter `name`, falling back to the source directory name; the package name is derived
from the selected path. Symlinks or special files inside a skill are errors; symlinks elsewhere are
ignored. Limits: 256 skills, 2000 files, 32 MiB total, 8 MiB per file, depth 12.

Packages are scoped: project (`.dial/`, the default) or user (`--user`). Precedence is project skills,
then user skills, then installed packages, then other tools' skill folders. A package cannot install a
skill name that another package already provides, and an already-installed package name is refused in
either scope.

Install scripts never run. Updates re-fetch the locked URL and ref and compare commits; a
matching commit reports "already up to date". Registry entries are not re-resolved on update, so an
update follows the URL and ref recorded at install time. `dial skills remove <name>` deletes the lock
entry and prunes the store.

Installed packages are recorded in `skill-packages.lock.json` (version 1) under the project's `.dial/`
directory or Dial's user directory.

### Registry protocol

Dial ships no default registry. Add one under `skills.registries` in settings (name → index URL).
`dial skills search <query>` and `/skills search <query>` query every configured registry. A failing
registry is reported on its own line and does not stop the others.

An index is one JSON document:

```json
{
  "version": 1,
  "skills": [
    {
      "name": "code-review",
      "description": "Review diffs for correctness",
      "url": "https://github.com/acme/dial-skills@v1.2#skills/code-review"
    }
  ]
}
```

- `version` must be `1`. Unknown fields are rejected.
- `url` uses the source syntax above. `dial skills install <registry-name>` resolves an entry by name.
- Schemes: `file://`, `https://`, `http://` only. Size cap 1 MiB. 30-second timeout. Redirects are
  not followed.

## Marketplaces and /extensions

`/extensions` is the one place to find and install MCP servers, plugins and skills. Its tabs are MCP servers, Plugins, Skills and Marketplaces. Listings load in the background when the browser opens. Press `/` to type a filter; MCP catalogs are searched when typing pauses, or immediately with Enter. Arrow keys leave the search field with the filter retained: Up/Down select results and Left/Right switch tabs. Page Up/Down and Home/End also navigate results; `/` resumes editing and Esc clears the filter. Enter in the search field finishes searching, without installing or re-fetching an already loaded query. On a result, Enter (or `i`) requests installation and then waits for confirmation. Adding, refreshing or searching an index installs nothing; removing, disabling and installing always wait for confirmation.

Search and setup fields accept pasted text without submitting it; single-line fields discard control characters. A rejected catalog request clears its loading state, and earlier install progress and setup forms remain subscribed until completion.

MCP result tags appear before descriptions so credential requirements are visible while browsing. Declared API key/token requirements are distinguished from optional credentials and ordinary required settings. `auth unknown` means the listing does not establish the server's authentication requirements, not that it is key-free. Smithery's platform API key is distinguished from the server's own settings. Open details with `v` for declared inputs and metadata limitations; no credential values are displayed in these labels.

Installed/imported MCP command displays omit arguments and remote URLs because either may carry credentials; the saved execution configuration is unchanged. Installations recheck name availability under the settings locks and refuse conflicts rather than overwrite a server installed by another session. Smithery names use the same bare-key normalization as other MCP catalogs.

A marketplace is a named index of plugins and skills. Dial bundles three curated skill catalogs:

| Marketplace | Upstream | Selection |
|---|---|---|
| `anthropic-skills` | [Anthropic skills](https://github.com/anthropics/skills) | Documents, spreadsheets, presentations, design, testing and skill authoring |
| `openai-skills` | [OpenAI skills](https://github.com/openai/skills) | GitHub review/CI, OpenAI docs, Playwright, security reviews/threat modeling, notebooks and Cloudflare deployment |
| `vercel-skills` | [Vercel agent skills](https://github.com/vercel-labs/agent-skills) | React/Next.js performance, composition and transitions, web interface/accessibility reviews, and writing guidelines |

These indexes ship in the binary: browsing and refreshing them requires no network and installs or executes nothing. They are curated selections, not complete mirrors of upstream repositories. Each entry targets its own skill directory (`#skills/<name>` or OpenAI's `#skills/.curated/<name>`). The added OpenAI and Vercel paths and frontmatter were checked against upstream `SKILL.md` files. Listings use package directory names; installed skill names may differ (for example, Vercel's `react-best-practices` package provides `vercel-react-best-practices`). Upstream skills may assume other harness tools or external dependencies; review the skill and its license before explicitly installing or using it. Refreshing a built-in catalog does not discover new upstream entries; catalog updates ship with Dial.

Add an index with the Marketplaces tab (`a`, then a name and an `https://` URL or local JSON file), `/marketplace add <name> <index>`, or `dial plugin marketplace add <name> <index>`. Turn one on or off with `e` (or `/marketplace enable|disable <name>`, `dial plugin marketplace enable|disable <name>`), and remove it with `x`. Removing a marketplace leaves what it installed in place.

Index format (`plugins` and `skills` are both optional):

```json
{
  "plugins": [
    {"name": "alpha", "version": "1.0.0", "source": "git+https://example.com/alpha.git#v1.0.0", "description": "Example plugin"}
  ],
  "skills": [
    {"name": "code-review", "description": "Review diffs for correctness", "url": "https://github.com/acme/skills.git#skills/code-review"}
  ]
}
```

- A plugin `source` is `git+<url>[#ref]`, `npm:<name>[@version]` or `file:<dir>`, as for `dial plugin install`.
- A skill `url` uses the skill package syntax (`url[@ref][#path]`), so the path and symlink checks above apply.
- Claude Code `.claude-plugin/marketplace.json` files are not read: their plugin sources are objects, not strings. Use a Dial-format index instead.

Marketplace state lives in `~/.dial/marketplaces/marketplaces.json`, which Dial writes. Each entry has `index`, `enabled` (default `true`) and `refreshed_at`. Downloaded snapshots sit beside it as `<name>.json`. Built-in catalogs use `index: "builtin"` and their marketplace name selects the bundled index. `seeded_builtins` tracks which catalogs were offered, so new catalogs appear once without restoring removed ones or re-enabling disabled ones. Migration honors the legacy `seeded` marker for the original Anthropic catalog and preserves existing custom indexes, including name collisions. Browsing does not write state; these migration markers persist on the next marketplace change. Removing a catalog never removes its installed skills.


## Configuration files

Repository files are never read as configuration. Settings come from the user's global `~/.dial/config.toml`, the per-project `config.toml` in Dial's state directory (`~/.dial/projects/<name>-<hash>/config.toml`, outside the workspace and user-owned), and command-line flags, merged in that order. Hooks and extensions may be configured in either file. `/settings sources` reports effective key provenance. Live reload keeps the last valid configuration when a changed file cannot be parsed.

## Plan mode

Plans are stored locally under Dial's state directory and exposed as `local://` artifacts. Plan mode uses the configured `planner` model role when one is defined, and restores the model in force before plan mode when leaving it. An approved plan is supplied as context when handing work to subagents.

## Durable tool contracts

Assistant tool batches record each admitted tool's name, semantic revision and canonical parameter/strictness hash in `dial_tool_contracts`. Code-mode calls also bind the admitted callback catalog. Documentation-only changes do not change identity. Session continuation and worker resume validate recorded identities against currently available tools. Saved calls are history, never instructions to replay effects; removed tools do not regain authority from old records.

Extensions increment `Tool::contract_revision()` when identical arguments acquire a new meaning. A changed tool must attest compatibility through `accepts_historical_contract` or provide `project_historical_call` for a settled historical call. A projector is checked for deterministic output and applied only to the provider-facing copy; repeating projection is idempotent after its identity advances. Unpaired calls and callback catalogs require explicit compatibility. Unsupported histories fail with an actionable diagnostic. Compatibility does not rewrite the original record or relax validation for new calls. Built-in identities start at revision 1; built-in semantic revision changes belong in `tools/contracts.rs`. Legacy unversioned sessions remain readable and use current validation for new calls.

Settled native edit calls remain readable when switching among Dial's five supported edit dialects. This compatibility accepts only known built-in revision-1 schemas, with no extension claiming the tool name. Future calls use the newly selected dialect. Training captures use the same projected history and tool schemas sent to the provider.

Execution results carry typed terminal status, effect certainty and committed paths separately from tool-owned payloads. MCP requests rejected before transport admission have no effects; timeouts, cancellation or transport loss after admission leave effects unknown. A terminal protocol reply settles the request. Unknown effects stop the shared lifecycle for inspection and never trigger automatic replay.

## Discover, import and toggle skills

Browse compatible skills from Claude, Codex, Agents (`.agents`/`.agent`) and pi
in this project and your home directory with `dial skills sources` or `/skills sources`.
Browsing does not activate them. Import portable `<name>/SKILL.md` folders explicitly
from those locations or any locally checked-out repository; no downloading or script execution.

```sh
dial skills sources
dial skills import ~/.claude/skills --dry-run
dial skills import ~/.claude/skills --user
dial skills import ~/.agents/skills --user
dial skills import ./vendor/skills/code-review
dial skills list
dial skills disable code-review          # this project
dial skills enable code-review --user    # user default; project overrides still win
dial skills import ~/.claude/skills --user --update --dry-run
dial skills import ~/.claude/skills --user --update
```

Sources can be one skill or a directory of immediate skill folders. Default
destination is `.dial/skills`; `--user` uses `$DIAL_HOME/skills` (normally
`~/.dial/skills`). Project skills shadow user skills. `/skills` lists installed skills,
their on/off state and location. Chat supports `/skills import <path> [--user] [--dry-run]
[--update]`, `/skills enable <name> [--user]` and `/skills disable <name> [--user]`.
Quote paths containing spaces. Invoke enabled skills with `/skill:<name>`.

Toggles persist by skill name in `skills.json` outside the workspace: project state by
default, or Dial's user directory with `--user`. A project setting overrides the user
default, including for same-name shadowed skills. Disabling never deletes a skill; it
removes it from completion, the model catalog and skill-tool loading. Imports and toggles
refresh chat completion immediately and the model catalog on the next turn. While a turn runs,
submitting a draft opens a choice between adding direction now and queuing for after the turn;
Option/Alt+Enter still queues a follow-up directly. Empty Enter with queued work opens a
keep-working/stop-and-send choice, defaulting to keeping the current work running. Sending the
oldest queued message requires explicitly choosing stop and confirming. Other queued messages
are preserved. Slash commands run immediately or are deferred. Previously loaded skill text
remains in conversation history; disabling is not a sandbox or a way to erase that text.

Folders need lowercase/digit/hyphen names and frontmatter with a nonempty
`description`; any declared `name` must match. Invalid entries and conflicts
are reported as skipped. `.dial-import.json` records source paths and hashes.
`--update` only replaces imports from the same source with no local content
changes. Updates are explicit; back up local edits before resolving conflicts.

Symlinks and special files are refused. Limits: 64 KiB per bundled file (80 KiB for the root
`SKILL.md`, including its metadata), 200 files,
8 MiB per skill, five nested directories. Executable permissions are not copied;
invoke reviewed scripts with their interpreter. Harness-specific tool names and
dependencies may need adaptation. Importing does not grant extra permissions.

For the recommended authoring pattern, versioned examples, loader budgets and utility evaluation,
see [Writing and evaluating skills](skills.md). Skills are procedures, not executable tools;
`/skill:<name>` requests guidance rather than running a script.

## Rust embedding SDK

The Rust SDK exposes `extensions::{Extension, ExtensionHost, Command}`, versioned `tools::ext::Tool`,
atomic tool registration and observational lifecycle events. Operator-trusted extensions can register
tools, prompt commands and render panels through `Ui`; executions still pass the agent's normal
validation/approval boundaries. It is a Rust embedding API, not a loader for arbitrary workspace plugins.

## Lifecycle hooks, path permissions, isolated workers and schedules

**Hooks.** `[[hooks]]` in your user or project-state config (never the repository) run programs at
`user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `stop`, `session_start`, `session_end`, `before_compact`, `before_subagent_spawn`, `before_provider_request`, `context`, `assistant_message`, `after_provider_response`, `session_before_branch` and `session_before_switch`. The event arrives as JSON on stdin.
Exit 0 continues (stdout is shown; for `post_tool_use` it is appended to the tool result), exit 2 blocks
(`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message, `stop` makes the agent continue with the reason for up to 3 rounds; stderr is the reason).
Other exits only produce a notice. On `pre_tool_use`, a timeout or missing program blocks by default (`fail_closed` defaults to true; set `fail_closed = false` for notice-only); on other events it only produces a notice. `matcher` is an anchored regex on
the tool name. Hooks are trusted and unsandboxed like `[mcp.*]`, apply to subagents too, are off in
sealed evaluations, and disable parallel read batching so every call passes `pre_tool_use`.
`before_provider_request` rewrites the model request before each call. `{"payload":..}` replaces the body; handlers chain in order, each seeing the previous replacement. A block (`{"block":..}` or exit 2) refuses that request with an error instead of sending it.

**More lifecycle events.** Aliases are accepted for the names that differ in other harnesses (`before_branch`, `before_switch`, `message_end`, `provider_response`).

- `context` fires before each provider request (before `before_provider_request`) with `{"messages":[..]}`, the request's message list. `{"messages":[..]}` in the answer replaces the list for that one request only (every entry needs a `role`; replacements over 256 KiB or malformed are skipped with a notice). Persisted history is never changed. A block refuses the request.
- `assistant_message` fires when a final assistant reply (one with no tool calls) is complete, with `{"text":..,"step":..}`. It is observe-only: the reply is already saved and cannot be replaced or withheld. A hook may return `additional_context`, which reaches the next request.
- `after_provider_response` is observe-only and fires after each main-session model response with `{"model","stop_reason","latency_ms","tool_calls","usage":{prompt_tokens,completion_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens}}`. Exit 2 or a block changes nothing.
- `session_before_branch` (`/branch`, `/undo`, `/retry` rewinds, `/fork`, ACP `fork_session`) and `session_before_switch` (`/new`, `/resume`, `/sessions`, RPC `new_session` and `resume`) are cancelable. The payload has `reason` and the target ids; exit 2 or `{"block":reason}` refuses the change and leaves the session as it is, showing the reason. Use `fail_closed = true` to make a crashing hook refuse too.
- `additional_context` (also `additionalContext`) is a result field on `pre_tool_use`, `post_tool_use`, `user_prompt_submit`, `assistant_message` and `context`. The text (clipped to 4 KiB per answer, repeats dropped, discarded when the same event blocks) becomes a transient harness note on the next model request. It is delivered once, is never written to session history, and never changes a tool result.

```toml
[[hooks]]
event = "pre_tool_use"
matcher = "write|edit"
command = ["sh", "-c", "grep -q '\\.env' && { echo 'no .env edits' >&2; exit 2; } || exit 0"]
timeout_secs = 0
```

**Path permission rules.** `/permissions deny --persist write --path .env --path 'secrets/**'` keeps the rule
(`--persist` must directly follow `allow` or `deny`). Globs are project-relative, a pattern without `/` matches at any depth, and matching is
case-insensitive on macOS. They apply to `read`, `write`, `edit` and `ast_edit`; hashline patch headers
are inspected, an undeterminable target is denied by deny rules and never granted by allow rules, and an
allow rule must cover every file a call touches. They do not cover `bash`, `grep` or `glob`.

**Isolated workers.** `task` accepts `isolation: "copy"` for mutating agents (`worker`, `test-runner`, and custom agents that set `mutates`); read-only agents are refused. The worker edits a private
copy of the project (dirty and untracked files included; `.git`, `.dial`, secrets and build caches
excluded). Afterwards only changed files are merged back, each only if the project's file is still what the
copy started from; others are reported as conflicts, never overwritten, and the full copy is kept. Merges
go through checkpoints (`/undo` works). On remote endpoints isolated workers run concurrently. This is a
file copy, not `git worktree`, so it works with dirty trees and non-git projects.

**Schedules.** `dial schedule add "prompt" --every 30m --watch 'src/**/*.rs' [--allow read,edit | -y]`,
`list`, `remove <id>`, `log <id>` and `run [--once]` (a foreground scheduler, one per project; keep it in
tmux or launchd). Runs are ordinary `dial -p` children, read-only unless `--allow`/`-y` was given at add
time, one at a time. Watch triggers wait for edits to settle and ignore the run's own changes.
The interactive `/schedule add` accepts the same repeated or comma-separated `--allow` values
and reports the resulting access policy when saving. `/schedule run` runs due entries once in a
background job; `dial schedule run` remains the persistent foreground scheduler unless `--once`
is supplied. Neither confirmation nor a named allow list grants approval to unlisted write tools.

## Git tools, budgets, custom agents and HTTP MCP

**`git` tool** (an integration, on by default). Structured `status`, `diff`, `log`, `show`, `branches`
(no approval) and `add`, `unstage`, `commit`, `branch_create`, `switch`, `push`, `pr_create` (approval),
plus `pr_view`, `pr_checks`, `issue_view` through the `gh` CLI. Arguments are validated and never pass
through a shell; external diff drivers and textconv are disabled, read actions run without repository hooks, and approved `commit` and `push` run repository hooks unless `git.bypass_hooks` is set; push never forces.
Path permission rules apply to `git` `paths`. Default prompt commands `/review`, `/commit-work` and `/pr` use
it; a file in `.dial/commands/` with the same name replaces them. The former `commit` prompt command is now `/commit-work`, since `/commit` is the commit command that generates the message and commits.

**Budgets.** `[budget] max_session_cost_usd` and `max_turn_cost_usd` stop a turn between model
requests. A turn's observed-cost allowance is shared across automatic verification repairs,
stop-hook continuations and auxiliary work; continuations do not reset it. These are observed-cost
boundaries, not invoice ceilings: missing pricing/usage and in-flight requests can add charges.
A model without known pricing cannot be stopped by an estimated charge (`/cost` shows pricing availability).
`[budget] warn_session_cost_usd` and `warn_turn_cost_usd` only notify: each warns once when spend first reaches the threshold, never stops work, and is read from the live configuration. Session spend includes subagents, since they charge the same ledger.

**Custom agents.** `[agents.<name>]` with `base` (`scout`, `reviewer` or `planner`), `prompt`,
optional `description`, `tools` (can only narrow the base's tools) and `max_steps`. Writers are not
configurable because approving a worker launch is decided without the configuration.

**Task model routing.** Delegation uses the first assigned model in a purpose-specific chain:
implementation workers try `task`, `coach`, then `small`; scouts and custom read-only agents try
`small`, then `task`; reviewers and test-runners try `tester`, `coach`, `task`, then `small`;
planners try `planner`, `coach`, `task`, then `small`. Main is used only if no helper in that chain
is assigned. This does not change maintenance roles or select unconfigured providers. Explicit
operator agent definitions and explicit per-call `model_profile` selections remain deliberate
overrides of the automatic role routing. See [Model routing](model-routing.md).

**HTTP MCP.** `[mcp.<name>]` takes `url` instead of `program` (https, or http on localhost) with
`bearer_env` naming an environment variable that holds a token, optional `headers`, and OAuth 2.1
sign-in (protected-resource and authorization-server discovery, dynamic client registration, PKCE)
via `/mcp login <name>`. Streamable HTTP with JSON or SSE replies and session ids is supported. `transport = "sse"` selects
the legacy HTTP+SSE transport: the event stream opens first, its `endpoint` event names where
requests are POSTed (it must stay on the server's origin), and replies arrive on that stream.
HTTP redirects are refused: configure the final endpoint URL so credentials and session IDs are
never forwarded to an unvalidated destination. OAuth refresh retries apply to HTTP 401 only;
HTTP 403 retains its status instead of being reported as an expired token.

**MCP discovery and install.** `/extensions` (MCP tab) immediately shows the curated list and loads
the first page from the official MCP registry, Smithery, npm and GitHub in the background (read
concurrently), even with no query. `/` filters loaded rows while typing; Enter searches every remote
catalog. The list scrolls
endlessly: moving the cursor within five rows of the end loads the next catalog page, and `n` still
loads it on demand. A failed page stops automatic paging until `r` refreshes. Servers that other
coding agents already configure (`.mcp.json`, `~/.claude.json`, `~/.codex/config.toml`, Cursor,
Gemini and Windsurf files) appear as `found` rows; `i` copies one into Dial's config after a
confirmation and a health check. Dial reads those files only to list them and never starts them on
its own. Remote MCP results for the submitted query retain the provider's relevance matching instead
of being hidden by an additional literal-text filter; rows whose name equals the query come first,
then rows whose name contains it, each group in listing order. The official registry requests latest versions
and follows its cursors; browsing is not limited to ten pages. Its search matches listing names
only. Smithery listings remain visible even
when their connection details are unavailable; those details are fetched when installing, not while
browsing. A server listed by several providers on a page appears once with every source badge:
listings fold together when their owner-qualified names match or their installs run the same npm
or PyPI package, and a GitHub row folds into the one listing that names its repository.
Provider failures are reported alongside any available results, not treated as a complete catalog.

npm and GitHub cover servers that are not published to a catalog. npm rows are packages tagged
`mcp` whose manifest declares a command (libraries are left out); they run as
`npx -y <package>@<version>`, or `npx -y --package=<package>@<version> <command>` when the package
has several commands and exactly one is named like the package or contains `mcp`. GitHub rows are
repository search results (an empty query browses `topic:mcp-server` by stars). A repository is
resolved only when installing: its root `package.json` or `pyproject.toml` must name a package
published on npm or PyPI whose metadata names that same repository as its source, and the package
must have one clear command (PyPI packages run as `uvx --from <package>==<version> <command>`).
Otherwise the row says why and links the repository to set up by hand. Neither listing declares
environment values, so a server that needs them fails the install health check and nothing is
saved. Anonymous GitHub search allows about ten searches a minute; `GITHUB_TOKEN` or `GH_TOKEN`
raises the limit and is sent only to `api.github.com`.
These catalogs are not an exhaustive list of every MCP server: a server must be published to one
of them to appear in remote search. `i` installs; installation always asks for confirmation first.
The curated list includes the [official Nuxt MCP server](https://nuxt.com/docs/4.x/guide/ai/mcp),
available as `nuxt` even without registry access. It uses Nuxt's documented stdio bridge,
`npx -y mcp-remote https://nuxt.com/mcp` (Node.js/npm required).
It also includes `graphify`, the local MCP server of [Graphify](https://github.com/Graphify-Labs/graphify),
which answers questions about a code knowledge graph: `query_graph`, `get_node`, `get_neighbors`,
`shortest_path`, `god_nodes` and more. It runs `uvx --from graphifyy[mcp] graphify-mcp` (uv required)
with `cwd = "."`, which resolves against the project root of each session (also under `-C`), so
one installed entry serves every project's own graph. It needs no account or key and reads
`graphify-out/graph.json` there (or under `$GRAPHIFY_OUT`). Build or refresh the graph in the project
first, for example `uvx --from graphifyy graphify extract . --code-only` (a local tree-sitter pass;
nothing leaves the machine), and consider adding `graphify-out/` to `.gitignore`. The server starts
without a graph and loads the file on each call, so a rebuilt graph is picked up without a
reconnect. The hosted `https://api.graphify.com/mcp` endpoint is a different server that needs a
Graphify Cloud account and `/mcp login`.
Required environment values are requested in a masked
form and saved in that server's `[mcp.<name>]` entry (`env`); Dial never guesses them. Servers that
need a header or a templated URL are not installed from here: the details pane names what to add by
hand. Each install starts the server once and saves nothing unless it starts and lists tools, with
one exception: a remote server that answers HTTP 401 with an OAuth (`WWW-Authenticate: Bearer`)
challenge is reachable but needs your account, so its `[mcp.<name>]` entry is saved and Dial tells
you to run `/mcp login <name>`; after the browser sign-in, `/mcp reconnect` connects it (`/mcp test
<name>` checks it). Dial never starts that sign-in by itself. A 401 without an OAuth challenge, a 403, or a
server configured with its own `Authorization` header or `bearer_env` still saves nothing. Imports
from other tools and Smithery installs follow the same rule. npm
and PyPI packages are pinned to the version the registry lists; `npx`, `uvx` or `docker` must be on
PATH. Config names are the last segment of the listing name (`io.github.acme/notes` → `notes`).

The agent has the same reach through `mcp_servers`: `list` (default), `search` with optional
`query` (reads listings only, downloads and starts nothing) and `install` with `id` from a search
result (`npm:<package>` and `github:<owner>/<repo>` ids resolve as above). Only `install` needs approval. It returns `needs_input` with the variable names when values
are required, and the user enters them in `/extensions`. It returns `needs_sign_in` after saving a
server that asks for an OAuth sign-in; the user then runs `/mcp login <name>`. An installed server takes effect in the
next session; `/mcp` shows its status. Smithery servers still install through their settings form
(`/extensions` → `i`, or `/mcp smithery-install`). Archived upstream packages (Brave, Slack,
GitLab, Puppeteer) are listed with that note because they still install.

MCP lists refresh while the session runs. A server that sends `notifications/tools/list_changed`, `resources/list_changed`, or `prompts/list_changed` is re-listed once: tool announcements are debounced for 100 ms and a refresh that is already running is not repeated, so a burst of announcements costs one `tools/list`. Tools change only at the next model request, never mid-request. A tool the server removed refuses calls with "removed by its server", and a tool whose definition changed gets a new identity and is offered from the next request. A failed re-list keeps the previous list and shows a notice. Each session keeps at most 64 pending events and 16 notices, dropping the oldest first; hook plugins that observe `mcp_notification` receive these events. Resource and prompt listings are cached until the server announces a change.

`/mcp prompt <server> <name> [key=value…]` fetches a prompt and puts its rendered messages in the composer for review; it is not sent. Arguments are split like shell words, so a value with spaces can be quoted (`topic="release notes"`); an unclosed quote or an argument without `=` is rejected with the usage line. Non-text content is shown as `[<type> content omitted]`. Output is sanitized before it reaches the composer. The fetch runs as a background job, so the UI stays responsive; when it finishes, the prompt is appended below any draft already in the composer. Cancel it with the usual job cancel, and only one background operation runs at a time.

## Structural guidance

Portable rules may declare `astCondition` (one pattern or a bounded list) and an optional
`astLanguage`. Structural matching inspects finalized source-bearing tool arguments before effects,
including native editor replacement and insertion fragments. It does not parse incomplete streamed
JSON or claim to lint the complete prospective file. File scopes use the editor's native target
parser, including sloppy-editor paths and rename destinations. AST rules cannot use judged-question
triggers. Invalid grammar and matching-budget failures stop the proposed execution.
