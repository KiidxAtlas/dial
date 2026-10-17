# Extensions and project context

Dial has two extension boundaries. Trusted operator integrations can register tools and commands through the Rust `Extension` SDK or through configured stdio extension programs. Workspace-provided guidance and skills are untrusted input: they are read-only, size-limited, and never execute programs. Dial never reads configuration from the workspace.

## Lifecycle hooks

Configure hooks with `[[hooks]]`; `event` names a lifecycle point, `command` is an argv array (never shell text), and `matcher` optionally selects tool calls by anchored regular expression. Hooks receive a JSON event on stdin. A JSON response can rewrite `input` before a tool call or `content` after a tool result, and can return a stop/block verdict where supported. Hook arrays append across configuration layers, preserving global hooks when project settings are loaded. The default deadline is 30 seconds. `fail_closed` defaults to true for `pre_tool_use` and false for observational events; set it explicitly when policy requires otherwise.

Supported lifecycle points include user prompt submission, session start/end, before compaction, before subagent spawn, before provider requests, pre/post tool use, and stop. Matchers are compiled and cached. A failing fail-closed policy hook prevents the action; hook failures must not be silently converted into permission.

## Custom tools

`[tools.<name>]` declares `program`, optional `args` and `timeout_secs`, plus `description` and a JSON Schema `parameters` object. When parameters are supplied, the tool receives the full JSON arguments object on stdin; otherwise it receives a single `input` string. Custom programs are operator-trusted and run outside the workspace sandbox. Do not configure workspace-controlled executables.

## Runtime extensions

Runtime plugins are operator-installed stdio programs: those declared under `[extensions.plugins.<name>]` and those installed with `dial plugin install` (started from the user `plugins` directory). Dial does not scan `~/.dial/extensions` or the project for plugin executables, so no project-local extension runs without an explicit configuration entry. Plugins communicate using JSON-RPC messages over stdio and may register tools, commands, shortcuts and flags, update active tools, and observe lifecycle events. They run with the privileges of the Dial process; only install code you trust. The Rust SDK is for embedding trusted native integrations and does not bypass tool approvals or workspace controls. Plugins and skills offered by marketplaces are found and installed in `/extensions`; see Marketplaces below.

## Context and skills

Instruction files such as `AGENTS.md` and `CLAUDE.md` are discovered from the project hierarchy. `@path` references in a user message are expanded into read-only, size-limited content confined to the project root (absolute, `..` and escaping symlinks are refused); instruction files themselves are not expanded. Dial also discovers supported context, command, and skill files from common coding-agent directories (including `.claude`, `.codex`, `.gemini`, `.cursor`, `.windsurf`, `.cline`, and `.github`). These files are model context, not executable configuration.

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

`/extensions` is the one place to find and install MCP servers, plugins and skills. Its tabs are MCP servers, Plugins, Skills and Marketplaces. Listings load in the background when the browser opens, and `/` then Enter searches every MCP source. Adding, refreshing or searching an index installs nothing. Installing is one Enter (or `i`) followed by a confirmation, and removing, disabling and installing always wait for that confirmation.

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
Enter queues steering for the running turn and Option/Alt+Enter queues a follow-up for the next
turn. With an empty composer, Enter sends the oldest queued message now: the current turn stops
at its next safe point and the message starts a new turn in the same chat. Other queued messages
are preserved. Slash commands run immediately or are deferred. Previously loaded skill text
remains in conversation history; disabling is not a sandbox or a way to erase that text.

Folders need lowercase/digit/hyphen names and frontmatter with a nonempty
`description`; any declared `name` must match. Invalid entries and conflicts
are reported as skipped. `.dial-import.json` records source paths and hashes.
`--update` only replaces imports from the same source with no local content
changes. Updates are explicit; back up local edits before resolving conflicts.

Symlinks and special files are refused. Limits: 64 KiB per file, 200 files,
8 MiB per skill, five nested directories. Executable permissions are not copied;
invoke reviewed scripts with their interpreter. Harness-specific tool names and
dependencies may need adaptation. Importing does not grant extra permissions.

## Rust embedding SDK

The Rust SDK exposes `extensions::{Extension, ExtensionHost, Command}`, versioned `tools::ext::Tool`,
atomic tool registration and observational lifecycle events. Operator-trusted extensions can register
tools, prompt commands and render panels through `Ui`; executions still pass the agent's normal
validation/approval boundaries. It is a Rust embedding API, not a loader for arbitrary workspace plugins.

## Lifecycle hooks, path permissions, isolated workers and schedules

**Hooks.** `[[hooks]]` in your user or project-state config (never the repository) run programs at
`user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `stop`, `session_start`, `session_end`, `before_compact`, `before_subagent_spawn` and `before_provider_request`. The event arrives as JSON on stdin.
Exit 0 continues (stdout is shown; for `post_tool_use` it is appended to the tool result), exit 2 blocks
(`pre_tool_use` refuses the call, `user_prompt_submit` rejects the message, `stop` makes the agent continue with the reason for up to 3 rounds; stderr is the reason).
Other exits only produce a notice. On `pre_tool_use`, a timeout or missing program blocks by default (`fail_closed` defaults to true; set `fail_closed = false` for notice-only); on other events it only produces a notice. `matcher` is an anchored regex on
the tool name. Hooks are trusted and unsandboxed like `[mcp.*]`, apply to subagents too, are off in
sealed evaluations, and disable parallel read batching so every call passes `pre_tool_use`.
`before_provider_request` rewrites the model request before each call. `{"payload":..}` replaces the body; handlers chain in order, each seeing the previous replacement. A block (`{"block":..}` or exit 2) refuses that request with an error instead of sending it.

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
requests. They rely on catalog pricing and reported usage, so a model without a known price is never
stopped by them (`/cost` shows whether pricing is available).

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
Required environment values are requested in a masked
form and saved in that server's `[mcp.<name>]` entry (`env`); Dial never guesses them. Servers that
need a header or a templated URL are not installed from here: the details pane names what to add by
hand. Each install starts the server once and saves nothing unless it starts and lists tools. npm
and PyPI packages are pinned to the version the registry lists; `npx`, `uvx` or `docker` must be on
PATH. Config names are the last segment of the listing name (`io.github.acme/notes` → `notes`).

The agent has the same reach through `mcp_servers`: `list` (default), `search` with optional
`query` (reads listings only, downloads and starts nothing) and `install` with `id` from a search
result (`npm:<package>` and `github:<owner>/<repo>` ids resolve as above). Only `install` needs approval. It returns `needs_input` with the variable names when values
are required, and the user enters them in `/extensions`. An installed server takes effect in the
next session; `/mcp` shows its status. Smithery servers still install through their settings form
(`/extensions` → `i`, or `/mcp smithery-install`). Archived upstream packages (Brave, Slack,
GitLab, Puppeteer) are listed with that note because they still install.

## Structural guidance

Portable rules may declare `astCondition` (one pattern or a bounded list) and an optional
`astLanguage`. Structural matching inspects finalized source-bearing tool arguments before effects,
including native editor replacement and insertion fragments. It does not parse incomplete streamed
JSON or claim to lint the complete prospective file. File scopes use the editor's native target
parser, including sloppy-editor paths and rename destinations. AST rules cannot use judged-question
triggers. Invalid grammar and matching-budget failures stop the proposed execution.
