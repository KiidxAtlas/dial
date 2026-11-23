# RPC and ACP

`dial rpc` speaks the newline-delimited JSON control protocol on stdin/stdout; `dial acp` exposes ACP v1 over stdio. Stdout is owned by one blocking writer thread. It uses a private unlinked spool when the consumer falls behind, preserving complete ordered frames without changing shared descriptor flags. Protocol producers reserve positions before mapping events, so concurrent notifications and replies cannot reorder.

Native RPC can negotiate `ready` with an empty `model` on first run. Clients can list sign-in providers and choose a model without sending an inference request. ACP still requires a configured model.

`--append-system-prompt TEXT` may follow the subcommand (`dial rpc --append-system-prompt …`), so a client that starts the engine can add standing guidance for its own surface; Dial Desktop uses it to tell the model which rich answer blocks it draws. Older engines refuse the flag after `rpc`; clients can check with `dial rpc --append-system-prompt x --help`, which exits 0 only when it is accepted.

RPC query methods remain available during active turns; commands that mutate or interrupt the active conversation retain their turn semantics. ACP requests are served by one core worker that runs one turn at a time: a prompt for a session other than the active one is refused until the running turn ends, and the running turn is never cancelled on another session's behalf. ACP permission requests are sent to the client as `session/request_permission`. Negotiated client filesystem methods are supported: when the client offers `fs.writeTextFile`, plain text writes use `fs/write_text_file`; with `fs.readTextFile` too, reads use `fs/read_text_file` so editor buffers are visible. Terminal, authentication and MCP bridge methods remain unsupported and are refused rather than executed with ambient authority. See the [client parity matrix](client-contracts.md#client-parity) and [uncertain editor writes](#uncertain-editor-writes).

[rpc-schema.json](rpc-schema.json) is a hand-maintained JSON Schema (draft 2020-12) of the plain `dial rpc` request, response and event frames; ACP uses JSON-RPC 2.0 and is not covered. Tests in `crates/dial-app/src/rpc/tests.rs` fail when its request methods, event names or limits drift from the implementation (method names are checked against the dispatch arms, event names against every emitted `"event"`). Print mode (`dial --print`) answers exactly one prompt per invocation: the positional words are joined with spaces and piped stdin is appended as context. `--image <path>` (repeatable) attaches images to that prompt through the same pipeline as `@path` in the TUI; if the selected model does not accept image input, the run fails before any request is sent. `--image -` reads one image from stdin instead (PNG, JPEG, GIF or WebP, detected from the leading magic bytes; oversized or non-image input is refused before any request is sent). Stdin then carries image bytes and is not appended as prompt text; a terminal stdin is refused, and at most one `-` is accepted. Stdout is drained before exit. `stream-json` omits repeated full-history snapshots.

For an image-bearing tool result, `data.artifact.uri` identifies its retained image. Native clients can request `read_image_artifact {uri}` even during a turn; it returns `media_type` and base64 `data` for a verified PNG, JPEG, GIF or WebP in this project's artifact store. Recovered tool messages carry the references in `dial_images`.

## Print mode and session automation

`dial -p --output-format json "prompt"` emits one result object; `--output-format stream-json`
emits newline-delimited system, text, tool, todo, notice and usage events followed by the result.
JSON modes reserve stdout for JSON. Results include session ID, answer, tool outcomes, usage and duration.
Use `--allow read,edit` for named tool approval instead of blanket `-y`; read-only mode and deny rules
still win. With both a prompt argument and piped stdin, stdin is appended as fenced context.
Print runs offer a smaller tool set than the terminal: `ask` is disabled, and `task`/`message`,
`todo`, `find` and `memory` are listed as loadable instead of offered,
unless `task.prefer_subagents=true` keeps `task`. The model can still `tool_load` any of them.
`eval` stays in the initial offer whenever containment and a cell runtime are available and it
fits the route's core budget (always on hosted routes).
Session `bg` jobs end when a print run exits. Before the run ends, Dial tells the model once which
explicitly started jobs are still running, and refuses `finish` once for them. A server the task
needs afterwards should be restarted with `bg persist=true`; otherwise the model should stop it.
Exit status: `0` completed, `1` error, `2` incomplete, blocked, needs input, no answer or interrupted, `3` a required tool approval was denied (takes precedence over 2).

`--output-schema FILE` makes the final answer structured: FILE is a JSON Schema checked before any request
(an unreadable, malformed or invalid schema fails the run with no model call). The answer is validated with the
same validator and bounded correction budget (`task.max_schema_retries`) that subagent `output_schema` uses;
fenced JSON is accepted. On success stdout carries the validated compact JSON (text mode prints it; `json` and
`stream-json` put it in a new `structured` field of the result object, next to `text`). A still-invalid answer
prints nothing, reports the validation error on stderr (and in the result's `error` field), and exits `2`.
`-o FILE` / `--output-last-message FILE` also writes the final answer (the validated JSON when a schema is in
force) to FILE through an atomic replace. When the turn produces no answer, FILE is neither created nor
overwritten. The flags need `-p`. Both are root-only: `dial sessions pack` keeps its own `-o`.

`dial review [--uncommitted | --base BRANCH | --commit SHA | PROMPT...]` runs the TUI `/review` workflow
non-interactively. With no target it reviews the uncommitted changes. The four target kinds are mutually
exclusive (clap rejects combinations), and refs must be single words. The target is frozen by the same review
code the TUI uses. The run is read-only: the approval policy is forced to `readonly` and reviewers have no
write tools. It honours `--output-format` (on the subcommand) and `-o`, and uses the print exit codes.

`dial sessions` lists this project's sessions by latest activity (`--all` lists every project). On a
terminal it prints a table (id, relative age, message count, title, and project with `--all`) with
the resume and export commands; piped output keeps the stable
`id  started  N msgs  title[  project]` lines for scripts. `dial sessions rename <id> <title>` and
`dial sessions export <id> [path] [--html]` expose session management without the TUI.

## Execution tree and result revisions

Execution uses shared, revisioned result bodies backed by the session's branch-aware JSONL journal.
RPC `state` exposes a `tree` containing session, conversation and execution nodes. Each execution
node has an `id`, `parent`, `kind`, `label` and `result` (`text`, optional structured `data`, `phase`,
`revision`). `state_reset` clears the current observer view; `state_node` replaces one node whenever
its metadata (label, kind, parent, call ID, presentation, phase, execution or structured data) changed,
and `state_delta` appends text to a running node at the supplied revision only when the text alone
grew. Worker updates use bounded
replacement snapshots. Legacy events remain available; clients should choose one projection to
avoid displaying a token twice. A client that renders the legacy events can stop the tree stream
with `hello {tree_events: false}` (see below). Tool-result events include original structured `data` when supplied.
Snapshots are written at boundaries, not for every token. Resuming never restarts unfinished workers.
Unassigned child roles inherit the parent model, profile and request controls; the model picker can
still assign an explicit child model.
Ctrl-C interrupts silent HTTP response reads without replacing the configured inference deadline with
a short timeout. Connection establishment and partial writes still use their configured deadlines.

RPC `state` replies come from one worker-owned projection, the same one the worker uses, and never include message history; read `messages` or `messages_page` for history. Typed RPC `notice` events keep their plain `message` and add `kind`, `lane` (`context`, `divider` or `alert`), `tokens` and, when the engine has them, `detail` as additive fields. `detail` is tagged by `type`: `model_switch` carries `from` (null when unknown), `to` and `reason` (`fallback`, `restore`, `promotion` or `prewalk`); `review` carries the advisor's `severity` and `note`, which are advice and never verified. Plain notices remain valid without these fields.

`messages_page` accepts an unsigned `offset` or a decimal-string `cursor` (offset takes precedence),
and an unsigned `limit` defaulting to 50 and clamped to 1–500. Invalid types, malformed cursors and
offsets past the snapshot end are errors, not a silent restart at page zero. An offset at the end
returns an empty page with no next offset.

## Persistent RPC and ACP transports

`dial rpc` is a persistent version-1 JSON-lines control endpoint. Requests have `id`, `method` and `params`;
responses correlate by `id`, while unsolicited events have `event`. Methods include `send`, `state`,
`messages`, `compact`, `seed_context`, `new_session`, `resume`, `rename`, `plan`, `steer`, `abort`,
`task_start`, `task_status`, `task_message`, `task_cancel`, `approve` and `shutdown`. Also: `set_model` (`provider`, `model_id`; the model must be in the bundled catalog, one of the configured current/role/favorite/profile models, a downloaded local model, or a chat model in a provider listing saved by an earlier `/models` refresh; models whose metadata declares embedding, rerank, image or audio use are refused, and any other selection is an error that leaves the model unchanged. Remote provider listings are not queried), `set_thinking` (`level`, one of the reasoning-effort names, or `default` to drop the session's level so the model's configured or catalog default applies again; unknown names are errors. A model without a `[model_profiles]` entry can still take a level when the bundled catalog or its saved provider listing declares reasoning levels; the profile Dial builds is session-only), `get_session_stats` (session ID, message count, cost, usage and metrics), `get_available_commands` (the built-in slash commands with `kind` `builtin`, then this project's custom commands (`custom`) and enabled skills (`skill`, named `skill:<name>`)), `export` (`format` `html` or `markdown`; optional relative `path` without `..`; default `dial-session-<id>.html` or `.md` in the project root; returns the written `path`), `set_host_tools` and `host_tool_result` (see below).

Native clients can pass `session_id` to `set_model` and `set_thinking`. A request for a session that is no longer active fails without changing the active session.

`send` text and ACP prompts that start with a custom command (`/name args`) or a skill (`/skill:name task`) are expanded exactly as in the terminal before the turn starts; an unknown skill is an error. A built-in command name (`/compact`, `/model`, …) is refused with an error naming the alternative, rather than sent to the model as if the command had run; clients use the matching method. Text that only happens to start with a slash (a pasted path) is sent as typed.

`send` attachments (native `dial rpc` only): `images` (paths: absolute, `~`, or relative to the workspace root) and `inline_images` (base64 image bytes, for pasted screenshots) attach images to the prompt through the same owner as `dial -p --image`. `@file` references in the text expand as in the terminal. A file that cannot be read, invalid base64, or a model that does not accept image input (`ready.image_input`) fails with `invalid_params` before the turn starts.

With a vision model, explicit `images` file attachments include the local path next to the image so the agent can reference the original in local tools. This path is visible to the inference provider. Pasted `inline_images` and tool images do not get a path label.

Live voice (`docs/voice.md`): `live_start` (optional `voice`, `instructions`), `live_mute` (`muted`), `live_stop`, and `live_listen` (records one utterance and runs it as a turn; not available over ACP). While a live session is active, `send` text gets its instructions, and each finished answer is read aloud.
Approval responses must name the currently pending `approval_id`; future/stale/replayed decisions fail.
The `approval` event carries `approval_id`, `tool`, `summary` and, when the request belongs to a call card, the call's `call_id`. `approval_resolved` (`approval_id`) follows once the request is decided, so clients can retire it.
Abort and task messaging/cancellation remain usable while inference or an approval wait is active.
Consumers must handle out-of-order response IDs and asynchronous events.

Host tools (`host_tools` capability, native `dial rpc` only): the host declares tools with `set_host_tools` (`tools`: 1–32 entries, each `name` matching `^[a-z0-9_]{1,64}$`, a 1–1024 character `description`, an object `parameters` JSON Schema and an optional `timeout_ms` from 1 to 600000, default 60000; names must be unique and must not collide with built-in tools). A second declaration in the same session is refused, and a refused declaration is not registered. Host tools belong to the session they were declared in: `new_session`, `resume` (of another session), `fork_session` and `close_session` (of the current session) drop them from the session being left and clear the registration, so the host declares again for the session it wants. Parked sessions never hold host tools. The model sees them as ordinary tools with their own schemas and contract identity. Each use needs normal approval (`approval` event) before the host is asked. When the model calls one, the server emits `host_tool_call` (`call_id`, `tool`, `arguments`) and the host answers with `host_tool_result` (`call_id`, then either `output`, a string, or `content`, an array of MCP content blocks, plus optional `is_error`). `content` is rendered as an MCP tool result is: text blocks verbatim, and the first PNG, JPEG, GIF or WebP image whose bytes match its `mimeType` is attached for models that accept images, the way MCP screenshots are. Giving both or neither is refused. `set_host_tools` replies `result_content: true` on engines that accept `content`. Answers name a call that is still pending; stale or unknown `call_id`s are refused. A call with no answer within `timeout_ms` fails with a timeout error to the model; aborting the turn fails the waiting call as cancelled, and so does a session switch that drops the tools. `host_tool_result` is handled by the controller, so it works while a turn is running. ACP does not expose `set_host_tools` or `host_tool_result`: ACP refuses them as unknown methods (`-32601`), and no host tools are offered in ACP sessions.

Host URI schemes (`host_uris` capability, native `dial rpc` only): `set_host_uri_schemes` (`schemes`: up to 16 unique names matching `^[a-z][a-z0-9+.-]{0,31}$`) declares the schemes the host serves. A later call replaces the list, and an empty list stops serving. The built-in and reserved schemes (`file`, `ssh`, `pr`, `issue`, `conflict`, `artifact`, `http`, `https`) are refused. Reading `<scheme>://…` with the `read` tool sends a `host_uri_request` event (`call_id`, `scheme`, `operation` `read`, `url`); the host answers with `host_uri_result` (`call_id`, `output`, `is_error`). Outputs are capped at 256 KiB, and an oversized output is an error. A read with no answer within 30 s, or one that is interrupted, fails. Reads of host schemes need approval, and `offset` and `limit` are rejected. Declarations are dropped on the same session switches as host tools.

Sign-in (`auth` capability, protocol 2 and native `dial rpc` only): `auth_providers` lists providers (`provider`, `name`, `signed_in`, `needs_terminal`; `needs_terminal` now only means the flow asks for typed values). `login` (`provider`) starts a sign-in; its response, sent when the flow finishes, is `{provider, message}`. While it runs the server emits `auth_url` (`provider`, `url`, `instructions`), `auth_progress` (`provider`, `message`) and `auth_prompt` (`provider`, `prompt_id`, `message`, `placeholder`, `allow_empty`, `secret`). `secret` is true when the answer is a credential (an API key, pasted code or cookie), so the client masks the field. Answer a prompt with `auth_code` (`prompt_id`, `code`). `auth_cancel` (`provider`) cancels a running sign-in. `logout` (`provider`) cancels a running sign-in for that provider, then signs out and replies `{provider, message}`. Only one sign-in runs at a time, and an unanswered prompt times out after 600 s. Tokens and entered codes are never echoed: the prompt event carries only its `prompt_id`, and the answer is not repeated in any event or result. Every provider can sign in this way, including those that type an API key, choose a region or paste a cookie.

MCP sign-in uses the same events: `mcp_login` (`server`) runs `/mcp login` for a configured remote server, with `provider` set to `mcp:<server>` on its events and for `auth_cancel`. It replies `{server, message}` when the flow ends (a local program has no sign-in and is refused; an unknown server is `not_found`). The grant applies when the server reconnects: `apply_settings {reconnect: true}`. `mcp_logout` (`server`) cancels a running sign-in and removes the stored grant, replying `{server, removed}`.

Session administration, models and settings (native `dial rpc` only for the `session_admin` and `settings` groups). These controller methods answer while a turn runs:

- `list_sessions` (`offset`, `limit` 1–500 default 50, `include_empty`): this project's sessions in picker order, pinned first then latest activity. Each entry has `session_id`, `title`, `started`, `updated`, `messages`, `user_messages`, `model`, `parent_id`, `first_message`, `last_message`, `last_answer`, `pinned`, `current` and `open_elsewhere`. The page has `offset`, `total` and `next_offset`. Sessions without a user request are omitted unless `include_empty` is true; the current session is always listed. Over ACP, `session/list` keeps its own ACP shape.
- `search_sessions` (`query`, optional `limit` 1–1,000 default 50): read-only full-text history
  search through the same local index as `/search`. The nonblank query is capped at 4,096 UTF-8
  bytes after trimming. Returns `{query, limit, hits, has_more}`; hits contain `session_id`,
  `entry_id`, `role`, `snippet`, bounded `preview`, `when` and best-effort `title` (empty when
  unavailable). Increase `limit` to expand the ranked window; refine the query beyond 1,000.
  Each request queries current history, not a stable snapshot. Runs on a bounded connection-owned
  read job while turns continue; saturation returns `busy`, malformed inputs `invalid_params`,
  and index failures `failed`. Search never opens or rewinds a conversation; use `resume` to open one.
- `pin_session` (`session_id`, `pinned`): idempotent; `changed` reports whether anything changed.
- `delete_session` (`session_id`): removes another session of this project together with its pin. The active session and journals held by another writer are refused.
- `fork_session` (optional `session_id`, default the active session): an actor method like `resume`, refused while a turn runs. It forks the session into a new one and switches to it, running the same branch and switch hooks as `/fork`. Returns `session_id`, `forked_from` and `plan_mode`.
- `list_models` (optional `provider`, optional `purpose` `chat` (default), `image` or `embedding`): every model `set_model` accepts without a network round trip. That covers the main, current, role, favorite and profile models, the bundled catalog of the providers they and `[providers]` name, downloaded local models, and the saved `/models` listings of those providers. Models whose metadata declares a non-chat kind (embedding, rerank, image, audio) are left out. Each row has `provider`, `model_id`, `label`, `sources`, `current` and `thinking_levels`, plus catalog or local metadata when known (`context_window`, `max_output_tokens`, `vision`, `reasoning`, `default_thinking`, `pricing`, `kind`, and `image_output`, true when the model also generates images). Live provider listings are not queried.
  With `purpose: "image"` the reply lists the models that generate images instead, for `image.model`: downloaded local models and saved `/models` listings of any provider whose metadata declares image output (the same test as the terminal's `/settings` picker; names are never guessed, and an explicit `disabled` excludes a model). Each row has `provider` (the id a role assignment saves), `provider_label` (the name to show, e.g. `gguf` for a local GGUF file on `mlx`), `model_id`, `label`, `sources`, `current` (it is the configured `image.model`), `base_url` and `image_api` (the `image.base_url` and `image.api` values that model needs). With `purpose: "embedding"` the reply lists, from the same sources, the models whose metadata declares the embedding kind (the terminal's embedding role rule), for the `embedding` role; rows have the same fields without `image_api`, and `current` marks the assigned `model_roles.embedding`. Another `purpose` is `invalid_params`.
- `image_settings` (no params): the image route `image_gen` resolves from the saved settings, for a client's image settings page. `ok: true` brings `model`, `api` (display name of the image protocol), `settings` (the `image.models` fields that protocol receives), `edits`, and `key` and `entry`: the `image.models.<key>` entry `/image settings` edits for that model (an empty one under a derived key when none exists). `ok: false` brings `error`, the reason generation would fail. Both carry `role` (the assigned `model_roles.image`, or null) and `options`: `sizes`, `qualities`, `backgrounds`, `moderations`, `max_steps`, `lora_scale_limit` and the installed MLX-Serve `loras`. Save changes with `set_setting` on `image.models.<key>` (the whole entry), then `apply_settings`.
- `delete_local_model` (`model`, optional `dry_run`): deletes a downloaded local model as Ctrl+D in `/models` → Get models does. `model` is a local model path or label that local discovery lists (Hugging Face cache, LM Studio, MLX-Serve); its whole folder in that store is removed. Replies `{label, model, folder, bytes, source, deleted}`; `dry_run: true` only reports. An unknown or out-of-store model is `not_found`, one the saved configuration or the session selects is `refused`, and a failed delete is `failed`.
- `settings_schema`: the settings reference (`dial config reference`): each setting's `path`, `label`, `description`, `kind`, `constraints`, `default` and `restart`.
- `get_settings` (optional `key`): every effective setting as `{key, value, redacted}`, or `{key, value}` for one key. Credentials are redacted.
- `set_setting` (`key`, non-null `value`, `scope` `global` or `project`, default `global`) and `reset_setting` (`key`, `scope`) use the same validation and locked write as `dial config set`/`reset`. They change the saved files only (`restart_required`): the running engine keeps its configuration until `apply_settings`. Values are never echoed.
- `apply_settings` (`settings` group, actor method): reloads the saved settings with the process's command-line flags and applies them to the running session, as the terminal does after `/settings` or `/reload`. An approval change applies in place; when nothing else changed the reply is `{applied: "live", rebuilt: false, approval}`. Any other change rebuilds the agent around the same session, keeping the conversation, todo list, plan mode, approval, permission rules, cost, a model chosen with `set_model`, the resident local model and background processes; MCP servers whose configuration is unchanged stay connected. The reply is `{applied: "rebuild", rebuilt: true, host_tools_reset, tools, model, image_input}`. `{reconnect: true}` rebuilds even when nothing changed and reconnects every MCP server instead of keeping its connection, as `/mcp reconnect` does; the reply adds `reconnected: true`. Host tools and URI schemes are dropped on a rebuild (`host_tools_reset`) and must be declared again; parked sessions must be `resume`d again. Invalid saved settings or an unreachable model fail with `invalid_params` and leave the agent unchanged; running workers fail it with `busy`. Engines without `apply_settings` can restart into the same session instead: `dial-client`'s `Client::restart_and_resume` does that.

`state` also carries `current_model` (`provider`, `model_id`, and `thinking` with the explicitly chosen `level` (null when none), `default_level` (the catalog's default when no level is chosen), the declared `levels`, and `settable`, which is true when `set_thinking` can choose a level for this model).

Checkpoints and undo (native `dial rpc` only, `checkpoints` group). These call the same owners as `/review`, `/changes` and `/undo`, through the session's checkpoint journal:

- `list_checkpoints`: checkpointed turns that changed files, newest first, each `{turn, label, files}`. A session without checkpoints lists none.
- `checkpoint_files` (`turn`): the files that turn changed, each with `path`, `writers`, `later_turns` and `overlap` (a reason string when another writer or a later turn touched the file, else null). Also returns `label` and `skipped`.
- `checkpoint_diff` (`turn`, `path`): the diff restoring that file would apply, and a `token` binding it to the file's current bytes.
- `restore_checkpoint_files` (`turn`, `files: [{path, token}]`, optional `overlap_confirmed`): restores the reviewed files, all or none. A stale token, or an overlapping file without `overlap_confirmed`, is `conflict`, and nothing is written.
- `undo` (optional `turns`, default 1): an actor method like `/undo`, refused while a turn runs. Restores the files of the dropped turns, then rewinds the conversation, keeping a summary of the abandoned branch. Returns `files` (`restored`, `removed`, `warnings`), `dropped`, `note`, `rollback_error`, `rewind_error`, `cleanup_error` and `conversation_kept` (true when a file rollback failed and the conversation was left intact). A session-change hook veto is `refused`. Nothing left to undo returns `dropped: 0`.

Extensions (native `dial rpc` only, `extensions` group): `list_extensions` returns the rows `/extensions` shows, read from the saved configuration, `{mcp: [row], skills: [row]}`, where each row has `id`, `title`, `summary`, `tags`, `origin` (`installed`, `curated`, `remote`, `foreign`), `state` (`connected`, `needssignin`, `failing`, `disabled`, `enabled`, `available`) and `details`.

`extension_action` (`intent`, optional `scope` `global`/`project`, optional `confirm`) runs one `/extensions` action through the overlay's own owner, including its health check and its rule for which config layer an existing server is changed in. `intent` is tagged by `op`: `install_curated_mcp {id}`, `install_mcp {id, values?}`, `toggle_mcp {id, enable}`, `remove_mcp {id}`, `test_mcp {id}`, `import_mcp {id}`, `remove_skill {id}`, `install_skill {source}`, `smithery_install {name}`, `smithery_key {name, key}`, `smithery_deploy {name, values}` and `catalog_browse {tab, query, page}`. Actions that change files, run a downloaded command, or remove or disable something need `confirm: true` (the overlay's confirm step) and are otherwise refused with `invalid_params`. The reply comes when the action finishes: `{ok, message, action, saved_scope, apply_settings, form, catalog}`. Progress lines arrive meanwhile as `extension_progress` (`action`, `message`). A saved change applies to the running session on `apply_settings`. When the owner needs values (a Smithery key, a server's settings, required inputs), `form` is `{submit, target, title, note, error, fields: [{key, label, hint, required, secret}]}`: ask the user, then send the intent named by `submit` with `target` and the answers. Typed values are never echoed. `catalog` is a browse page (`tab`, `query`, `page`, `rows`, `more`, `message`, `error`). One action runs at a time; another is refused with `busy`.

Session control (native `dial rpc` only, `session_control` group; actor methods, refused while a turn runs):

- `set_approval` (`mode`: `ask`, `write`, `auto`, `readonly` or `yolo`): sets the session's approval mode, as `/approval` does. Unlike `auto`, `yolo` also permits eligible tool calls recovered from model text without a one-time prompt. Returns `{approval, description}`. `state.approval` reports the current mode.
- `retry`: rewinds the unanswered last prompt (a failed or interrupted turn) and sends it again, as `/retry` does. The reply has the shape of `send`. Fails with `invalid_params` when the last message is not an unanswered prompt.
- `restart_model`: unloads this session's Dial-managed local model; it reloads on the next request. External endpoints are never restarted (`invalid_params`).

Permission rules (native RPC): `list_permission_rules` returns the active `rules` with zero-based `index`, `effect`, `tool`, `command_prefix`, `paths` and a readable `scope`. `add_permission_rule` takes `effect` (`allow` or `deny`), `tool`, optional `command_prefix` (shell words) or `paths` (path globs), and optional `persist`. `remove_permission_rule` takes a returned `index` and optional `persist`. A change is session-only unless `persist: true` is requested. Re-list before removing a rule, since indices change after removal.

Workspace directories (native `dial rpc` only, `workspace` group): `list_dirs` (controller method, answers during a turn) returns `{root, dirs}`, also reported as `state.workspace`. `add_dir` and `remove_dir` (`path`: absolute, `~`, or relative to the primary root; actor methods) change the workspace as `/add-dir` and `/remove-dir` do, persist it, and return `{changed, path, dirs}`.

Background jobs (native `dial rpc` only, `jobs` group; controller methods, answer during a turn): `list_jobs` returns the session's `bg` jobs, each `{name, command, status, running, stop_requested, terminal}`. `stop_job` (`name`) stops one and returns `{name, stopped}`; an unknown job is `not_found`. Attaching to a job's terminal stays terminal-only.

Usage (native `dial rpc` only, `usage` group; controller method, answers during a turn): `usage` returns `{providers: [{provider, plan, report, error}]}` for every signed-in provider with a usage endpoint, as `/usage` shows. `plan` is the account's readable plan or tier (`Plus`, `Pro`, `Max 20x`, a Copilot or Z.AI plan, a Gemini tier) or `null` when the provider does not report one; `report` is the provider's limits, notes and metadata. One provider's failure is its `error`; the others still report. Spending a saved reset (`/usage reset`) stays terminal-only.

Checks, evidence, policy, skills and reliability (native `dial rpc` only; ACP does not serve them). Checks: `list_checks` (read) returns what `/checks` shows: `{enabled, changed_files, automatic, configured}`; the automatic plan comes from the uncommitted changes, and an empty or non-repository tree reports none. `add_check {name, command, file_extensions?, timeout_secs?, confirm: true}` and `remove_check {name, confirm: true}` (write) edit the project check list as `/checks add` and `/checks remove` do; a check is a shell command that runs after edits, so both need `confirm: true`, and the change applies on `apply_settings`. Evidence: `evidence` (read) answers `checks` (the checks recorded at turn completion), `check {name}` (the retained log of one recorded check by exact name), `inspect {reference}` and `lineage {references}`; it writes nothing. `evidence_correct {reference, correction}` (write) appends a human correction, which is also added to the conversation as unverified evidence data. `evidence_compare {reference, other}` (write, because it stores a new investigation record) runs the semantic comparison; unlike `/evidence compare`, its result is not added to the conversation, so the agent sees it only when the client sends it as a message. Evidence text is redacted as `/evidence` shows it; redaction covers the patterns the storage redactor knows (for example bearer authorization headers), not every secret format. Policy and reliability: `policy` (`/policy`) and `reliability` (`/reliability`) are reads of the adaptive request policy and the tool reliability report for this session. Skills: `list_skills {action?: list|sources|installed|search, query?}` (read) answers the `/skills` views with the owner's text; `search` fetches each configured registry index over the network, so it is the one read that leaves the machine, and its query is limited to eight words and 200 bytes. `skill_action {action, name?, spec?, path?, scope?: project|user, dry_run?, update?, confirm?}` (write) runs one `/skills` change on a worker thread, one at a time: `enable` runs at once, `disable`, `install`, `update`, `remove` and a non-dry-run `import` need `confirm: true`, and a second change while one runs is refused `busy`. Typed fields become owner arguments; no shell line is parsed. Refusals use the coded errors above (`invalid_params` for a malformed or unconfirmed request, `conflict` for a duplicate check, `not_found` for an unknown check, `busy` for a concurrent skill change).

Check edits judge duplicates and missing names against the saved files, under the settings locks, not the session's snapshot. `add_check` for a name the user or project file already defines is `conflict`, and `remove_check` for a name the project file lacks is `not_found`. A removed name can be added again before `apply_settings`, and a repeated add never writes a second entry. `list_checks.changed_files` reads Git's NUL-delimited status, so paths with spaces, non-ASCII names or `->` text are reported verbatim.

Code history and statistics (native `dial rpc` only, `diagnostics` capability; both run off the agent, so they answer during a turn). `code_history {action?: search|show, query?, path?, branch?, limit?, id?}` (read) answers `/code-history`: `search` (the default) returns `{action, versions}` newest first, filtered by words, a `path` file or directory and a `branch`, with `limit` 1–200 (default 30); `show {id}` returns `{action, version, diff}` or `not_found`. It is `refused` while `codebase.history` (or `codebase.enabled`) is off. Clearing the history deletes every recorded diff, so `action: "clear"` is `refused` and stays a confirmed terminal action (`/code-history clear`). `stats {range?}` (read) answers `/stats <range>` with `{range, report}`: session transcripts are synced into the local statistics index, then reported for `24h`, `7d`, `2w`, `30d` or all time; no provider is contacted.

Schema (`schema` group, controller method): `get_schema` returns this protocol's JSON Schema, the same document as `docs/rpc-schema.json`, so a client in any language can generate request and event types from the engine it is connected to.

Command parity: `get_available_commands` gives every discoverable slash command an `rpc` field saying where a client finds it: `{surface: "method", method}` (call that method), `{surface: "client", reason}` (presentation the client renders itself, such as themes, key bindings or the activity panel) or `{surface: "terminal", reason}` (only the terminal serves it today; the reason names the alternative when there is one). Custom and skill commands are `{surface: "method", method: "send"}`. A test fails when a new command is added without a classification.

### Native workflow operations

These operations are advertised only by native RPC, not ACP. Check `ready.methods`; use the
published JSON Schema for exact tagged variants. Actor mutations return `busy` during a main
turn rather than silently queueing a session change behind it.

- **Modes:** `get_modes` inspects state; `set_mode` controls vibe, skillful, plan, prewalk,
  advisor, external thinking and plan-yolo through their existing session owners. Drafter,
  fast/ultra/slow and extended-context preferences use the same settings plans as the terminal
  and adopt through a validated rebuild. Drafter is project-scoped; fast/slow and extended
  context retain the terminal's global scope. Unsupported provider tiers are refused.
- **Goals:** `goal {action: get|set|guided|pause|resume|drop|budget, ...}` uses the existing
  coordinator. Actions that return a continuation run through the ordinary turn lifecycle,
  with cancellation and compute admission; they are not completion shortcuts.
- **Side conversations:** `side_thread` supports `list`, `ask`, `open`, `answer` and `cancel`.
  During a main turn, asking uses its frozen pre-turn context, not half-written history.
  `side_state` events report persisted streaming prefixes and structured exchange outcomes.
  Configurations requiring Dial's foreground compute admission refuse side inference over RPC
  rather than borrowing or reacquiring an actor lease. Externally managed endpoint routes remain
  subject to the existing lease-exemption policy; this is not a blanket loopback restriction.
  `side_promote {thread_id}` creates a sibling conversation without switching the main session.
  Switching, rewinding, compaction and relocation invalidate old snapshots. Clipboard handling
  remains the client's job. Handoff uses `compact {method:"handoff", require_method:true,
  instructions?:"..."}` through the existing compaction owner.
- **Session maintenance:** `archive {view: projects|sessions|prompts, ...}` returns structured
  history. `pack_session {path?}` refuses overwriting an existing archive. `label_session
  {label}` labels the active leaf (empty clears it). `branch_session {turns, confirm:true}`
  rewinds conversation only, never restores files. `move_session {path, confirm:true}` validates
  the destination before relocation and replies only after rebuilding with the destination
  layout/configuration; controller queries then use that same destination.
- **Schedules:** `list_schedules`, `add_schedule`, `remove_schedule` and `schedule_status`
  reuse the persisted scheduler store and lease. Add/remove require explicit `confirm:true`;
  confirmation does not grant write permissions. Schedules remain read-only unless the request
  specifies allowed capabilities. Status probes the existing lease; it does not start a daemon.
- **Learning:** `research_status` reports stored versions, metrics, admission evidence and the
  session's actual version/steering state. `research_action` evaluates a pending candidate or
  promotes an evaluated steering vector with explicit confirmation and the existing admission
  gates. Responses distinguish unchanged evaluation, next-turn adoption when improvement is
  enabled, stored admission while improvement is disabled, and steering's next-model-load
  activation when enabled. No request silently enables improvement or steering.
- **Collaboration:** `collaboration_start {confirm:true,read_only?:bool}` hosts the current
  conversation using the existing encrypted room owner; read-only is the default.
  `collaboration_invite {confirm:true}` deliberately returns a secret bearer link; clients must
  not log or expose it. Status and events never include that link. `collaboration_stop` tears
  down the hosted room. Hosting ends on disconnect or conversation replacement.
  `collaboration_status`, `collaboration_join {link,name?,confirm:true}` and
  `collaboration_leave` also use the room owner. Joining means admission is pending, not that
  the guest can write. Guest prompts and aborts retain host admission checks and never fall back
  to local inference. Relay service administration remains outside this API.

`recovery_snapshot` returns a combined idle transcript and live-state snapshot with an event
sequence fence. It refuses active workers and oversized histories rather than certifying a
racing or truncated recovery. Process preceding messages before applying it. `state.live`
contains only pending approvals, question and todo with their fence; it cannot certify transcript
recovery. See [client recovery](client-contracts.md#rust-client).

`steer` accepts input only for an active turn, not merely a busy actor, and any supplied session ID
must match that turn. Admission closes when the turn settles. Unconsumed queued input is discarded
with a `notice` of `kind: "steering_discarded"` and `count`; it never leaks into the next turn. Send
that input again explicitly if still wanted. Session activation clears prior live state and reseeds
the activated session's todos. Unchanged side-conversation polls reuse their current projection.

### One process per session

Each `dial rpc` process is the single writer of its active session's journal. A GUI with several tabs runs one engine process per open session. A second process that resumes a session already open elsewhere gets a fork (`forked_from` names the original) instead of sharing the journal, `list_sessions` marks such sessions `open_elsewhere`, and `delete_session` refuses them with `busy`. The owning process continues undisturbed.

### Event envelope

Every plain-RPC event (not replies, not ACP frames) carries `seq`, `session_id` and `request_id`:

- `seq` starts at 1 with `ready` and increases by one per event in stdout order. A gap means frames were lost: refresh `state` and `messages_page` instead of trusting an incrementally built view. `dial-client` counts gaps in `Client::lost_events`.
- `session_id` is the session the engine was serving when it produced the event. An event that already names a session keeps its own value.
- `request_id` is the id of the request whose processing produced the event, or null for background work between requests.

`ready` also carries `engine_version`, the engine build version. Use it for display and diagnostics. Decide features from `methods` and `capabilities`, never by comparing versions.

Tool cards: after `tool_call`, the engine emits `tool_card` (`call_id`, `label`, `target`, `tone`, `body`, `facts`, optional `preview`), the same card the terminal renders. It can be sent again for the same `call_id` as the call progresses; the latest card replaces the earlier one. `tool_result` adds `status` (`ok`, `error`, `blocked`, `skipped`, `timeout`, `aborted`), `effects` (`none`, `known`, `unknown`) and `changed_paths` (absolute paths). Render these fields instead of parsing `summary` or `text`.

Tool output and progress: `tool_output` (`call_id`, `name`, `delta`) streams text into the row of the call named by `call_id`; `call_id` is empty when the dispatch had no id, and clients then fall back to `name`. `tool_progress` (`call_id`, `name`, `text`) carries the latest live status snapshot of one running call and replaces the previous one. ACP maps both to `tool_call_update` on the same tool call: progress shows after the streamed output and never erases it. Stream-json print output carries `call_id` on `tool_output` when the dispatch had one. A retry that discards a failed attempt's streamed answer emits a typed `notice` with `kind` `retry` and `lane` `context`, after the partial answer is marked interrupted. Worker retries do not emit it.

When a client stops reading stdout, the engine queues frames (in memory, then spooled to disk) rather than dropping them. The client receives them in order once it reads again. The private spool is capped at 1 GiB of outstanding bytes; delivered space is reused even when traffic keeps the queue continuously nonempty. The same bound limits the spool file's physical size. Exhausting the outstanding-byte budget fails the connection rather than silently dropping frames.

### Ready frame

The first event on every connection is `ready`. Besides `protocol_version` (the version this connection speaks: `1` until the client negotiates otherwise), `session_id`, `model`, `tools`, `image_input` and `question_input`, it carries what a client needs to negotiate and size its requests:

- `supported_protocol_versions`: array of versions the server serves (currently `[1, 2]`). Refuse to continue when none is one you implement.
- `methods`: request methods this transport accepts. `capabilities`: the capability groups those methods belong to (`prompt`, `query`, `plan_mode`, `sessions`, `session_admin`, `maintenance`, `context_seed`, `tasks`, `steering`, `abort`, `approvals`, `questions`, `live_voice`, `model`, `export`, `host_tools`, `host_uris`, `auth`, `settings`, `checkpoints`, `extensions`, `schema`, `session_control`, `workspace`, `jobs`, `usage`, `benchmarks`, `checks`, `evidence`, `diagnostics`, `skills`, `shutdown`). Over ACP the `questions`, `live_voice`, `host_tools`, `host_uris`, `auth`, `session_admin`, `settings`, `checkpoints`, `extensions`, `schema`, `session_control`, `workspace`, `jobs`, `usage`, `benchmarks`, `checks`, `evidence`, `diagnostics` and `skills` groups are absent. The method table, capability groups and error codes are defined once in the `dial-protocol` crate, which the engine and `dial-client` share.
- `limits`: `max_request_bytes` (1 MiB for RPC, 8 MiB for ACP; a longer line is drained and answered with an error), `max_pending_steering` (8), `max_seed_messages` (200), `max_seed_message_bytes` (1 MiB), `max_memory_command_bytes` (8192), `messages_page_default` (50), `messages_page_max` (500) and `tool_result_event_bytes` (24 KiB; `tool_result` event text is clipped, the model-visible result is not).

`hello` also takes `interactive` (boolean, default false; native `dial rpc` only, echoed in the reply). A GUI where a person answers sets it to true: the engine may then ask for things it otherwise refuses over RPC, such as the directory-access grant the model requests to work outside the workspace (sent as an ordinary `approval` event). Scripts leave it false and keep the non-interactive behaviour. `dial-client`'s `Launch::interactive` sets it.

`hello` also takes `tree_events` (boolean, default true; echoed in the reply). A client that renders the legacy `text`/`reasoning`/tool events sets it to false: the engine then stops sending `state_reset`, `state_node` and `state_delta` on that connection, so streamed text is not sent twice, and instead emits `inference_done` (`id`, the execution-tree node id) once when each top-level model request completes successfully, the model-round boundary `state_node` gave. The `state` query still returns the tree. Engines that leave `tree_events` out of the reply predate the option and keep sending tree events. `dial-client`'s `Launch::tree_events` sets it.

Protocol negotiation (`hello`, `params.protocol_version`): send one `hello` as the first request to move the connection to the highest version you speak. The response carries the negotiated `protocol_version` and `supported_protocol_versions`. A client that never sends `hello` stays on version 1 and sees exactly the v1 frames. Version 2 changes one thing: failed requests carry a stable `error.code` next to `error.message`. Codes are:

- `busy`: another operation owns the session (a turn or maintenance is running, the session is closing) or the steering queue is full. Admin reads (`list_sessions`, `list_models`, `settings_schema`, `get_settings`, `list_extensions`, `list_checks`, `list_skills`, `get_schema`) and the `usage` poll run as connection-scoped read jobs, at most four in flight at once; a read past that bound is refused with `busy` at once rather than queued. When the connection ends, new reads are refused, cancellable reads (`usage`) receive the connection's cancel token, and answers not yet delivered are dropped; non-cancellable reads finish on their own under their owner's limits. Retry after the current operation settles.
- `invalid_params`: the request is malformed or a value is out of range (missing `text`, `approval_id` or `protocol_version`, an unknown approval `decision`, an invalid history cursor, an unparsable question payload or answer, a built-in command name sent as a prompt).
- `not_pending`: the targeted turn, approval or question is not, or no longer, pending (`steer` with no active turn, a stale or already answered `approval_id`, a mismatched, unacknowledged, expired or replayed question). Drop the stale card rather than retrying.
- `not_found`: the named session, setting, checkpoint turn or file does not exist.
- `refused`: a policy or hook declined the operation (for example a session-change hook vetoing `undo`). Nothing changed.
- `conflict`: the target changed since the client last read it (a stale checkpoint review token, an unconfirmed overlap). Read again before retrying.
- `unknown_method`, `unsupported_protocol_version` (no served version is at or below the requested one) and `internal_error` (the actor failed; the session is terminated).
- `failed`: every other request failure.

Version 1 errors carry `message` only. Version 2 clients should branch on `code`, not on message text, and treat unknown codes as `failed`.

For Rust frontends, the `dial-client` crate implements this negotiation, request correlation and disconnect accounting; see [client contracts](client-contracts.md#rust-client).

All three are derived from the same constants the reader and handlers enforce.

`dial acp` exposes the ACP initialize/session/prompt/cancel/permission/mode flow over bounded JSON-RPC
stdio with streamed assistant/tool updates. Loaded sessions retain exclusive writer ownership; parked
sessions are not concurrently writable. ACP does not advertise unsupported filesystem/terminal
capabilities. Both automation transports use bounded serialized stdout and stop honestly on stalled
or broken consumers instead of reporting partially delivered success.

The implementation keeps the controller (`rpc.rs`) separate from the agent-owning actor
(`rpc/actor.rs`). Only the actor mutates sessions or performs model/tool work; the controller
serves cached queries, routes approvals and questions, and initiates cancellation/cleanup.
Each parked ACP session owns its tools and plan mode in one record, so switching and closing
cannot leave independently keyed execution state behind. This separation does not change wire
methods, approval authority, queue limits or cancellation deadlines.

Every activation runs the cancelable `session_before_switch` hook before cancelling
outgoing work or removing a parked session, including implicit ACP selection by a
session-scoped request. A veto leaves the active session and parked writer intact.
Forking runs both the branch and switch gates. Activation restores only the incoming
session's saved, still-existing directory roots and rebinds its checkpoint observer;
outgoing session roots, sensitive native-access grants and file handles do not carry
over. Invalid incoming workspace/checkpoint setup fails before outgoing work is torn down.

ACP renders `notice`, `background` and `fatal` events as unchanged `agent_message_chunk` text. Each chunk also carries standard `_meta.dial` with `event` and, only when present, `kind`, `lane`, `tokens`, `name` and `replace`, so clients can tell sources apart. This is metadata preservation only; ACP does not implement client-side replacement of background output.

ACP sessions offer three `configOptions`: `mode` (Agent or Plan), `model` (values are `provider/model` from the same offline listing as `list_models`) and, when `set_thinking` can choose a level for the model, `thought_level`. Its first choice, `default` ("Model default", with the catalog default level in parentheses when known), drops the session's level so the model default applies again. Changing the model or level answers with the updated options and a `config_option_update`. A prompt that changed the model or mode (for example through `exit_plan`) also sends `config_option_update`. `session/request_permission` offers Allow once, Allow always, Allow always in this project (`project`) and Deny.

ACP correlates tool calls and final results by the RPC `call_id`, not the tool name, so overlapping
calls to the same tool retain distinct output and completion status. An approval with a `call_id`
attaches to that call's card; an approval without a matching card gets a placeholder that the call's
own `tool_call` adopts or that `approval_resolved` removes. Name-only legacy output is attached to
the latest active call of that name; it does not override explicit call identities.

RPC `tool_timing` events also carry `call_id`, alongside `name` and `seconds`. Concurrent calls to
the same tool publish their own timing and result as each finishes; clients should correlate by
call ID rather than attach a duration to the most recently displayed tool name.

Native RPC resume of a held writer lease creates an independent sibling and returns `forked_from`.
ACP `session/load` and `session/resume` require the exact requested identity: contention or a
moved-away source is refused without creating a sibling or changing the active session. Integrity
and non-contention I/O errors also fail rather than forking.

A prompt response maps the authoritative outcome. Completed turns return a plain `stopReason` of
`end_turn`; interrupted turns return `cancelled`; a turn that used its whole step budget returns
`max_turn_requests`. Failed, blocked, needs-input and incomplete turns return `end_turn` with
`_meta.dial.outcome` (the Dial outcome) and `_meta.dial.report` (the final report text). Because ACP
stop reasons cannot say this, such a turn also ends with a visible `agent_message_chunk`
("Dial: the turn ended as needs input.", with `_meta.dial.event` `outcome`). The RPC prompt
result carries the same information as `outcome` and a boolean `step_limit`.

ACP plan entries map each todo status explicitly: `pending`, `in_progress` and `completed` map directly;
abandoned tasks are `completed` and blocked tasks are `pending`. ACP has no status for either, so
their entry text is prefixed `[abandoned]` or `[blocked]` with the task note appended in
parentheses, and `_meta.dial.status` and `_meta.dial.note` carry the same values.

In-flight ACP request IDs are rejected before prompt preparation or cancellation has side effects.
Cancellation requests with IDs receive their own replies; cancellation notifications never produce
responses and cannot consume the pending prompt's final response. Cancelling a parked session does
not interrupt the active session.

### Uncertain editor writes

ACP client filesystem writes wait up to 30 seconds for the editor's response. Refusal before
delivery has no write effects; timeout, cancellation, lost acknowledgement or an error after
delivery leaves effects unknown. Diagnostics retain the originating call identity and never
claim that the editor did not write merely because Dial did not observe success. No local write
is substituted after a client error, and confirmed earlier files in a batch remain reported.

On the current connection, an uncertain write blocks another write to that path. Its late response
remains correlatable after cancellation or prompt completion. Only acknowledgement followed by
a successful fresh read releases the barrier; a read racing an unacknowledged write cannot do so.
The pending-write registry is bounded at 128 paths and refuses further writes when full. A client
that never acknowledges requires inspection rather than automatic replay. Reconnecting is not
evidence that an earlier request finished; these in-memory correlations are not a crash journal.

## Building a client

See [client compatibility and conformance](client-contracts.md) for the version policy, schema-checked
request fixtures, cancellation/reconnect handling, revision rules and an executable contract matrix.
The [workflow examples](../examples/workflows/README.md) compose existing skills, session exports and
verified learned-artifact packs without adding a transport or workflow interpreter.
